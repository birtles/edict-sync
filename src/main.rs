#[macro_use]
extern crate failure;
extern crate memchr;
extern crate quick_xml;
extern crate smallvec;
#[macro_use]
extern crate structopt;

use failure::{Error, ResultExt};
use smallvec::SmallVec;
use std::path::PathBuf;
use std::str;
use std::str::FromStr;
use structopt::StructOpt;
use quick_xml::reader::Reader;
use quick_xml::events::{BytesText, Event};

#[derive(StructOpt)]
#[structopt(name = "jmdict-couch")]
/// Perform an incremental update of a CouchDB representation of the JMDict database using the
/// supplied JMDict XML file.
struct Opt {
    #[structopt(short = "i", long = "input", help = "Input file", parse(from_os_str))]
    input: PathBuf,
}

type InfoVec = SmallVec<[String; 4]>;
type PriorityVec = SmallVec<[String; 4]>;

/// entry from jmdict schema
#[derive(Debug)]
struct Entry {
    /// ent_seq
    id: u32,
    /// k_ele children
    kanji_entries: Vec<KanjiEntry>,
    /// r_ele children
    reading_entries: Vec<ReadingEntry>,
    /// sense children
    senses: Vec<Sense>,
}

/// k_ele from jmdict schema
#[derive(Debug)]
struct KanjiEntry {
    /// keb
    kanji: String,
    /// ke_inf
    info: InfoVec,
    /// ke_pri
    priority: PriorityVec,
}

/// r_ele from jmdict schema
#[derive(Debug)]
struct ReadingEntry {
    /// reb
    kana: String,
    /// re_nokanji
    no_kanji: bool,
    /// re_restr
    related_kanji: Vec<String>,
    /// re_inf
    info: InfoVec,
    /// re_pri
    priority: PriorityVec,
}

/// sense from jmdict schema
#[derive(Debug, PartialEq)]
struct Sense {
    /// stagk
    only_kanji: Vec<String>,
    /// stagr
    only_readings: Vec<String>,
    /// pos
    part_of_speech: Vec<String>,
    /// xref
    cross_refs: Vec<CrossReference>,
    /// ant
    antonyms: Vec<CrossReference>,
    /// field
    field: Vec<String>,
    /// misc
    misc: Vec<String>,
    // s_inf
    // sense_info: Option<String>,
    // lsource
    // lang_sources: Vec<LangSource>,
    // dial
    // dialect: Option<String>,
    /// gloss
    glosses: Vec<String>,

    /// The language of this sense.
    /// In JMDict this is annotated onto each gloss, but all glosses for a given sense have the same
    /// language so we move this to the sense because it's more compact and allows us to create
    /// per-language views more easily.
    lang: Option<String>,
}

#[derive(Debug, PartialEq)]
struct CrossReference {
    kanji_or_reading: String,
    reading: Option<String>,
    sense_index: Option<u8>,
}

/*
struct LangSource {
    lang: String,
    original: Option<String>,
}
*/

fn main() {
    let opt = Opt::from_args();

    let entries = get_entries(&opt.input);
    if let Err(ref e) = entries {
        use std::io::Write;
        let stderr = &mut ::std::io::stderr();
        writeln!(stderr, "{}", e).expect("Error writing to stderr");
        ::std::process::exit(1);
    }

    let entries = entries.unwrap();

    /*
    for entry in entries {
        println!("> {:?}", entry);
    }
    */
    println!("Parsed {} entries", entries.len());
}

fn get_entries(input: &PathBuf) -> Result<Vec<Entry>, Error> {
    let mut reader = Reader::from_file(input).context("Could not read from file")?;
    reader.trim_text(true);
    reader.check_end_names(false);
    reader.expand_empty_elements(true);

    let mut buf = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();

    loop {
        match reader.read_event(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name() {
                b"entry" => {
                    entries.push(parse_entry(&mut reader)?);
                }
                _ => (),
            },
            Ok(Event::Eof) => break,
            Err(e) => bail!(
                "Error parsing entry at position #{}: {}",
                reader.buffer_position(),
                e
            ),
            _ => (),
        }
        buf.clear();
    }

    Ok(entries)
}

fn parse_entry<T: std::io::BufRead>(reader: &mut Reader<T>) -> Result<Entry, Error> {
    let mut id: u32 = 0;
    let mut kanji_entries: Vec<KanjiEntry> = Vec::new();
    let mut reading_entries: Vec<ReadingEntry> = Vec::new();
    let mut senses: Vec<Sense> = Vec::new();

    let mut buf = Vec::new();
    let mut ent_seq = false;

    loop {
        match reader.read_event(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name() {
                b"ent_seq" => {
                    ensure!(
                        !ent_seq,
                        "Nested ent_seq at position #{}",
                        reader.buffer_position()
                    );
                    ent_seq = true;
                }
                b"k_ele" => kanji_entries.push(parse_k_ele(reader)?),
                b"r_ele" => reading_entries.push(parse_r_ele(reader)?),
                b"sense" => senses.push(parse_sense(reader)?),
                _ => warn_unknown_tag(e.name(), reader.buffer_position(), "entry"),
            },
            Ok(Event::End(ref e)) => match e.name() {
                b"entry" => break,
                b"ent_seq" => {
                    ensure!(
                        ent_seq,
                        "Mismatched ent_seq tags at position #{}",
                        reader.buffer_position()
                    );
                    ent_seq = false;
                }
                _ => (),
            },
            Ok(Event::Text(e)) => {
                if ent_seq {
                    id = u32::from_str(&e.unescape_and_decode(&reader)?)
                        .context("Failed to parse ent_seq as int")?;
                }
            }
            Err(e) => bail!(
                "Error parsing entry at position #{}: {}",
                reader.buffer_position(),
                e
            ),
            _ => (),
        }
        buf.clear();
    }

    ensure!(
        id != 0,
        "ID not found at position #{}",
        reader.buffer_position()
    );
    ensure!(
        !reading_entries.is_empty(),
        "No reading entries found at position #{}",
        reader.buffer_position()
    );

    Ok(Entry {
        id,
        kanji_entries,
        reading_entries,
        senses,
    })
}

fn parse_k_ele<T: std::io::BufRead>(reader: &mut Reader<T>) -> Result<KanjiEntry, Error> {
    let mut kanji: String = String::new();
    let mut info: InfoVec = InfoVec::new();
    let mut priority: PriorityVec = PriorityVec::new();

    enum Elem {
        Keb,
        KeInf,
        KePri,
    }
    let mut elem: Option<Elem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name() {
                b"keb" => elem = Some(Elem::Keb),
                b"ke_inf" => elem = Some(Elem::KeInf),
                b"ke_pri" => elem = Some(Elem::KePri),
                _ => warn_unknown_tag(e.name(), reader.buffer_position(), "k_ele"),
            },
            Ok(Event::End(ref e)) => match e.name() {
                b"k_ele" => break,
                _ => elem = None,
            },
            Ok(Event::Text(e)) => match elem {
                Some(Elem::Keb) => kanji = e.unescape_and_decode(&reader)?,
                Some(Elem::KeInf) => info.push(parse_single_entity(e.escaped(), reader)?),
                Some(Elem::KePri) => priority.push(e.unescape_and_decode(&reader)?),
                _ => warn_unexpected_text(&e, reader, "k_ele"),
            },
            Err(e) => bail!(
                "Error parsing entry at position #{}: {}",
                reader.buffer_position(),
                e
            ),
            _ => (),
        }
        buf.clear();
    }

    assert!(
        kanji.trim() == kanji,
        "Kanji keys should not have leading or trailing whitespace"
    );
    ensure!(
        !kanji.is_empty(),
        "Kanji key is empty at position #{}",
        reader.buffer_position()
    );

    Ok(KanjiEntry {
        kanji,
        info,
        priority,
    })
}

fn parse_r_ele<T: std::io::BufRead>(reader: &mut Reader<T>) -> Result<ReadingEntry, Error> {
    let mut kana = String::new();
    let mut no_kanji = false;
    let mut related_kanji: Vec<String> = Vec::new();
    let mut info: InfoVec = InfoVec::new();
    let mut priority: PriorityVec = PriorityVec::new();

    enum Elem {
        Reb,
        ReRestr,
        ReInf,
        RePri,
    }
    let mut elem: Option<Elem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name() {
                b"reb" => elem = Some(Elem::Reb),
                b"re_nokanji" => no_kanji = true,
                b"re_restr" => elem = Some(Elem::ReRestr),
                b"re_inf" => elem = Some(Elem::ReInf),
                b"re_pri" => elem = Some(Elem::RePri),
                _ => warn_unknown_tag(e.name(), reader.buffer_position(), "r_ele"),
            },
            Ok(Event::End(ref e)) => match e.name() {
                b"r_ele" => break,
                _ => elem = None,
            },
            Ok(Event::Text(e)) => match elem {
                Some(Elem::Reb) => kana = e.unescape_and_decode(&reader).unwrap(),
                Some(Elem::ReRestr) => related_kanji.push(e.unescape_and_decode(&reader).unwrap()),
                Some(Elem::ReInf) => info.push(parse_single_entity(e.escaped(), reader)?),
                Some(Elem::RePri) => priority.push(e.unescape_and_decode(&reader).unwrap()),
                _ => warn_unexpected_text(&e, reader, "r_ele"),
            },
            Err(e) => bail!(
                "Error parsing entry at position #{}: {}",
                reader.buffer_position(),
                e
            ),
            _ => (),
        }
        buf.clear();
    }

    assert!(
        kana.trim() == kana,
        "Kana keys should not have leading or trailing whitespace"
    );
    ensure!(
        !kana.is_empty(),
        "Kana key is empty at position #{}",
        reader.buffer_position()
    );

    Ok(ReadingEntry {
        kana,
        no_kanji,
        related_kanji,
        info,
        priority,
    })
}

fn parse_sense<T: std::io::BufRead>(reader: &mut Reader<T>) -> Result<Sense, Error> {
    let mut only_kanji: Vec<String> = Vec::new();
    let mut only_readings: Vec<String> = Vec::new();
    let mut part_of_speech: Vec<String> = Vec::new();
    let mut cross_refs: Vec<CrossReference> = Vec::new();
    let mut antonyms: Vec<CrossReference> = Vec::new();
    let mut field: Vec<String> = Vec::new();
    let mut misc: Vec<String> = Vec::new();
    let mut glosses: Vec<String> = Vec::new();
    let mut lang: Option<String> = None;

    enum Elem {
        SenseTagKanji,
        SenseTagReading,
        PartOfSpeech,
        CrossReference,
        Antonym,
        Field,
        Misc,
        Gloss,
    }
    let mut elem: Option<Elem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event(&mut buf) {
            Ok(Event::Start(ref e)) => match e.name() {
                b"stagk" => elem = Some(Elem::SenseTagKanji),
                b"stagr" => elem = Some(Elem::SenseTagReading),
                b"pos" => elem = Some(Elem::PartOfSpeech),
                b"xref" => elem = Some(Elem::CrossReference),
                b"ant" => elem = Some(Elem::Antonym),
                b"field" => elem = Some(Elem::Field),
                b"misc" => elem = Some(Elem::Misc),
                b"gloss" => {
                    elem = Some(Elem::Gloss);
                    for a in e.attributes() {
                        if let Ok(attr) = a {
                            if attr.key == "xml:lang".as_bytes() {
                                // XXX Do proper error handling here
                                let lang_str = (str::from_utf8(&(attr.value))?).to_owned();
                                match lang {
                                    Some(ref current_lang_str) => {
                                        ensure!(*current_lang_str == lang_str,
                                                "All glosses within a sense should use the same language");
                                    }
                                    _ => lang = Some(lang_str),
                                };
                            }
                        }
                    }
                }
                // _ => warn_unknown_tag(e.name(), reader.buffer_position(), "sense"),
                _ => (),
            },
            Ok(Event::End(ref e)) => match e.name() {
                b"sense" => break,
                _ => elem = None,
            },
            Ok(Event::Text(e)) => match elem {
                Some(Elem::SenseTagKanji) => {
                    only_kanji.push(e.unescape_and_decode(&reader).unwrap())
                }
                Some(Elem::SenseTagReading) => {
                    only_readings.push(e.unescape_and_decode(&reader).unwrap())
                }
                Some(Elem::PartOfSpeech) => {
                    part_of_speech.push(parse_single_entity(e.escaped(), reader)?)
                }
                Some(Elem::CrossReference) => cross_refs.push(parse_cross_ref(
                    &e.unescape_and_decode(&reader).unwrap(),
                    reader.buffer_position(),
                )?),
                Some(Elem::Antonym) => antonyms.push(parse_cross_ref(
                    &e.unescape_and_decode(&reader).unwrap(),
                    reader.buffer_position(),
                )?),
                Some(Elem::Field) => {
                    field.push(parse_single_entity(e.escaped(), reader)?)
                }
                Some(Elem::Misc) => {
                    misc.push(parse_single_entity(e.escaped(), reader)?)
                }
                Some(Elem::Gloss) => glosses.push(e.unescape_and_decode(&reader).unwrap()),
                // _ => warn_unexpected_text(&e, reader, "r_ele"),
                _ => (),
            },
            Err(e) => bail!(
                "Error parsing entry at position #{}: {}",
                reader.buffer_position(),
                e
            ),
            _ => (),
        }
        buf.clear();
    }

    Ok(Sense {
        only_kanji,
        only_readings,
        part_of_speech,
        cross_refs,
        antonyms,
        field,
        misc,
        glosses,
        lang,
    })
}

#[test]
fn test_parse_sense() {
    let xml = r#"<sense>
                 <stagk>延べる</stagk>
                 <stagk>伸べる</stagk>
                 <gloss>to postpone</gloss>
                 <gloss>to extend</gloss>
                 </sense>"#;
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let _ = reader.read_event(&mut buf);
    assert_eq!(
        parse_sense(&mut reader).unwrap(),
        Sense {
            only_kanji: vec!["延べる".to_owned(), "伸べる".to_owned()],
            only_readings: vec![],
            antonyms: vec![],
            part_of_speech: vec![],
            cross_refs: vec![],
            glosses: vec!["to postpone".to_owned(), "to extend".to_owned()],
            lang: None,
        }
    );
}

/// Take a string like "&ent;" and return "ent".
//
// What I'd really like to do here is have something like:
//
// ```ignore
// trait ParseEntity<E>: E {
//   fn parse(src: &str) -> Result<E>;
// }
//
// enum KanjiInflection {
//   ... have the contents and impl of ParseEntity produced by a mako template from a simple
//       list of strings...
// }
//
// pub fn parse_single_entity<E>(raw: &[u8]) -> Result<E, Error> where E: ParseEntity<E>
// {
//   ... throws when the value doesn't match
// }
//
// Then we wouldn't need to decode at all and we could just pass integers around. But setting up the
// build system to run mako is probably overkill for this.
fn parse_single_entity<T: std::io::BufRead>(
    raw: &[u8],
    reader: &mut Reader<T>,
) -> Result<String, Error> {
    // Check we start with &, end with ;, and have nothing inbetween.
    if !raw.starts_with(b"&") || !raw.ends_with(b";") || memchr::memchr(b'&', &raw[1..]).is_some()
        || memchr::memchr(b';', &raw[..raw.len() - 1]).is_some()
    {
        bail!(
            "Error parsing entity at position #{}",
            reader.buffer_position(),
        )
    }

    Ok(reader.decode(&raw[1..raw.len() - 1]).into_owned())
}

fn parse_cross_ref(input: &str, buffer_position: usize) -> Result<CrossReference, Error> {
    if input.is_empty() {
        bail!("Empty cross-reference at position #{}", buffer_position);
    }

    let parts: Vec<&str> = input.split('・').collect();

    // Simple case, no separators
    if parts.len() == 1 {
        return Ok(CrossReference {
            kanji_or_reading: input.to_owned(),
            reading: None,
            sense_index: None,
        });
    }

    // The middle dot can either be the separator of the kanji / reading / sense OR it can just be
    // the regular separator in a katakana word.

    // If the last part is an integer, assign the sense.
    let sense_index: Option<u8> = parts.last().unwrap().parse::<u8>().ok();
    let non_sense_parts = if sense_index.is_some() {
        parts.len() - 1
    } else {
        parts.len()
    };

    // Assign the other parts depending on if we're likely looking at a katakana word or a regular
    // entry.
    let mut reading: Option<String> = None;
    let kanji_or_reading = if is_katakana(parts.first().unwrap()) {
        parts[0..non_sense_parts].join("・").to_owned()
    } else {
        if non_sense_parts > 2 {
            bail!(
                "Error parsing cross-reference at position #{}: {}",
                buffer_position,
                input,
            );
        }
        // Assign the reading if we have one
        if non_sense_parts == 2 {
            reading = Some(parts[1].to_owned());
        }
        (*parts.first().unwrap()).to_owned()
    };

    Ok(CrossReference {
        kanji_or_reading,
        reading,
        sense_index,
    })
}

#[test]
fn test_parse_cross_ref() {
    assert_eq!(
        parse_cross_ref("集束", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "集束".to_owned(),
            reading: None,
            sense_index: None,
        }
    );
    assert_eq!(
        parse_cross_ref("因・2", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "因".to_owned(),
            reading: None,
            sense_index: Some(2),
        }
    );
    assert_eq!(
        parse_cross_ref("如何・どう", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "如何".to_owned(),
            reading: Some("どう".to_owned()),
            sense_index: None,
        }
    );
    assert_eq!(
        parse_cross_ref("何方・どちら・1", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "何方".to_owned(),
            reading: Some("どちら".to_owned()),
            sense_index: Some(1),
        }
    );
    assert_eq!(
        parse_cross_ref("ブロードノーズ・セブンギル・シャーク", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "ブロードノーズ・セブンギル・シャーク".to_owned(),
            reading: None,
            sense_index: None,
        }
    );
    // I'm not sure if this actually exists, but it seems possible.
    assert_eq!(
        parse_cross_ref("カタカナ・コトバ・2", 0).unwrap(),
        CrossReference {
            kanji_or_reading: "カタカナ・コトバ".to_owned(),
            reading: None,
            sense_index: Some(2),
        }
    );
}

fn is_katakana(word: &str) -> bool {
    word.chars().all(|c| c >= '\u{30a0}' && c <= '\u{30ff}')
}

#[test]
fn test_is_katakana() {
    assert_eq!(is_katakana("トマト"), true);
    assert_eq!(is_katakana("トマト・パスト"), true);
    assert_eq!(is_katakana("ﾄﾏﾄ"), false);
    assert_eq!(is_katakana("とまと"), false);
}

fn warn_unknown_tag(elem_name: &[u8], buffer_position: usize, ancestor: &str) {
    match str::from_utf8(elem_name) {
        Ok(tag) => println!(
            "WARNING: Unrecognized {} member element {} at position #{}",
            ancestor, tag, buffer_position
        ),
        _ => println!(
            "WARNING: Unrecognized {} member element (non-utf8) at position #{}",
            ancestor, buffer_position
        ),
    }
}

fn warn_unexpected_text<T: std::io::BufRead>(text: &BytesText, reader: &Reader<T>, ancestor: &str) {
    match text.unescape_and_decode(reader) {
        Ok(text) => println!(
            "WARNING: Unexpected text \"{}\" in {} element at position #{}",
            text,
            ancestor,
            reader.buffer_position(),
        ),
        _ => println!(
            "WARNING: Unexpected text in {} element (non-utf8) at position #{}",
            ancestor,
            reader.buffer_position()
        ),
    }
}
