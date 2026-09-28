//! Text extraction from local files. Plain-text formats are read as UTF-8; `.docx` is unzipped and
//! its body text pulled from the XML. Everything is bounded, so a huge file or a zip bomb is skipped
//! rather than read. Nothing runs external tools.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{Limits, Skip};

/// Extensions read as plain UTF-8 text.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "text", "md", "markdown", "mdx", "rst", "adoc", "org", "csv", "tsv", "json", "jsonl",
    "yaml", "yml", "toml", "ini", "cfg", "conf", "xml", "log", "tex", "rs", "py", "js", "jsx",
    "ts", "tsx", "go", "java", "kt", "swift", "c", "h", "cc", "cpp", "hpp", "cs", "rb", "php",
    "sh", "bash", "zsh", "ps1", "sql", "tf", "hcl", "proto", "graphql", "svelte", "vue", "css",
    "scss",
];

/// Extension-less file names read as plain text.
const TEXT_NAMES: &[&str] = &["readme", "license", "makefile", "dockerfile", "changelog"];

/// The format a path would be read as, from its name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Docx,
}

/// The format of `path`, or the reason it isn't read.
pub fn format_of(path: &Path) -> Result<Format, Skip> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("docx") => Ok(Format::Docx),
        Some("pdf") => Err(Skip::PdfNotSupported),
        Some(e) if TEXT_EXTENSIONS.contains(&e) => Ok(Format::Text),
        None => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            if TEXT_NAMES.contains(&name.as_str()) {
                Ok(Format::Text)
            } else {
                Err(Skip::Unsupported)
            }
        }
        Some(_) => Err(Skip::Unsupported),
    }
}

/// Extracts the text of `path`. Blank results are a [`Skip::Empty`].
pub fn extract(path: &Path, limits: &Limits) -> Result<String, Skip> {
    let format = format_of(path)?;
    let size = std::fs::metadata(path)
        .map_err(|e| Skip::Unreadable(e.to_string()))?
        .len();
    let text = match format {
        Format::Text => {
            if size > limits.max_file_bytes {
                return Err(Skip::TooLarge(size));
            }
            let bytes = std::fs::read(path).map_err(|e| Skip::Unreadable(e.to_string()))?;
            decode_text(&bytes)?
        }
        Format::Docx => {
            if size > limits.max_docx_bytes {
                return Err(Skip::TooLarge(size));
            }
            docx_text(path, limits.max_docx_xml_bytes)?
        }
    };
    if text.trim().is_empty() {
        return Err(Skip::Empty);
    }
    Ok(text)
}

/// UTF-8 text without a BOM. NUL bytes mean binary.
fn decode_text(bytes: &[u8]) -> Result<String, Skip> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    if bytes.contains(&0) {
        return Err(Skip::NotText);
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| Skip::NotText)
}

/// The body text of a `.docx`: `word/document.xml`, read up to `max_xml` bytes uncompressed.
fn docx_text(path: &Path, max_xml: u64) -> Result<String, Skip> {
    let file = File::open(path).map_err(|e| Skip::Unreadable(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| Skip::NotText)?;
    let entry = archive
        .by_name("word/document.xml")
        .map_err(|_| Skip::NotText)?;
    if entry.size() > max_xml {
        return Err(Skip::TooLarge(entry.size()));
    }
    // The declared size can lie; cap what is actually inflated too.
    let mut xml = Vec::new();
    entry
        .take(max_xml + 1)
        .read_to_end(&mut xml)
        .map_err(|e| Skip::Unreadable(e.to_string()))?;
    if xml.len() as u64 > max_xml {
        return Err(Skip::TooLarge(xml.len() as u64));
    }
    let xml = String::from_utf8(xml).map_err(|_| Skip::NotText)?;
    Ok(wordml_text(&xml))
}

/// Text of WordprocessingML: the contents of `<w:t>` runs, a newline per paragraph, tabs and breaks
/// kept. Enough for search; formatting, headers and footnotes are ignored.
fn wordml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut in_text = false;
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        if in_text {
            out.push_str(&decode_entities(&rest[..lt]));
        }
        let Some(gt) = rest[lt..].find('>') else {
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        let self_closing = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        match (name, tag.starts_with('/'), self_closing) {
            ("w:t", false, false) => in_text = true,
            ("w:t", true, _) => in_text = false,
            ("w:p", true, _) => out.push('\n'),
            ("w:tab", false, _) => out.push('\t'),
            ("w:br" | "w:cr", false, _) => out.push('\n'),
            _ => {}
        }
        rest = &rest[lt + gt + 1..];
    }
    out
}

/// Decodes the five XML entities and numeric character references.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn limits() -> Limits {
        Limits::default()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn docx(dir: &Path, name: &str, xml: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(b"<Types/>").unwrap();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn formats_come_from_the_name() {
        assert_eq!(format_of(Path::new("a/notes.MD")), Ok(Format::Text));
        assert_eq!(format_of(Path::new("spec.docx")), Ok(Format::Docx));
        assert_eq!(format_of(Path::new("README")), Ok(Format::Text));
        assert_eq!(format_of(Path::new("deck.pdf")), Err(Skip::PdfNotSupported));
        assert_eq!(format_of(Path::new("photo.png")), Err(Skip::Unsupported));
        assert_eq!(format_of(Path::new("binary")), Err(Skip::Unsupported));
    }

    #[test]
    fn text_files_are_read_as_utf8_without_a_bom() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "a.txt", "\u{feff}Azure tenant — ok".as_bytes());
        assert_eq!(extract(&p, &limits()).unwrap(), "Azure tenant — ok");
    }

    #[test]
    fn binary_blank_invalid_and_oversized_text_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "a.txt", b"abc\0def");
        assert_eq!(extract(&p, &limits()), Err(Skip::NotText));
        let p = write(dir.path(), "b.txt", &[0xff, 0xfe, 0x41]);
        assert_eq!(extract(&p, &limits()), Err(Skip::NotText));
        let p = write(dir.path(), "c.txt", b"  \n\t\n");
        assert_eq!(extract(&p, &limits()), Err(Skip::Empty));
        let p = write(dir.path(), "d.txt", &[b'x'; 64]);
        let small = Limits {
            max_file_bytes: 63,
            ..limits()
        };
        assert_eq!(extract(&p, &small), Err(Skip::TooLarge(64)));
        let missing = dir.path().join("gone.txt");
        assert!(matches!(
            extract(&missing, &limits()),
            Err(Skip::Unreadable(_))
        ));
    }

    #[test]
    fn docx_body_text_is_extracted_with_paragraphs_tabs_and_entities() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:r><w:t>Hosting:</w:t></w:r><w:r><w:tab/><w:t xml:space="preserve"> Azure &amp; on-prem</w:t></w:r></w:p>
            <w:p><w:r><w:t>Auth &lt;SAML&gt; &#8212; &#x2713;</w:t><w:br/><w:t>line two</w:t></w:r></w:p>
            <w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr></w:p>
        </w:body></w:document>"#;
        let p = docx(dir.path(), "spec.docx", xml);
        assert_eq!(
            extract(&p, &limits()).unwrap(),
            "Hosting:\t Azure & on-prem\nAuth <SAML> — ✓\nline two\n\n"
        );
    }

    #[test]
    fn a_docx_that_inflates_past_the_cap_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let body = "<w:p><w:r><w:t>x</w:t></w:r></w:p>".repeat(2000);
        let p = docx(dir.path(), "big.docx", &body);
        let tight = Limits {
            max_docx_xml_bytes: 1000,
            ..limits()
        };
        assert!(matches!(extract(&p, &tight), Err(Skip::TooLarge(_))));
    }

    #[test]
    fn a_broken_docx_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), "fake.docx", b"not a zip");
        assert_eq!(extract(&p, &limits()), Err(Skip::NotText));
    }

    #[test]
    fn entities_decode_and_stray_ampersands_survive() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(
            decode_entities("AT&T &bogus; &#65;&#x42;"),
            "AT&T &bogus; AB"
        );
        assert_eq!(decode_entities("&#xD800;"), "&#xD800;");
        assert_eq!(decode_entities("tail &"), "tail &");
    }
}
