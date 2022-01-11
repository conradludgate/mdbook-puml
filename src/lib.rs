use anyhow::{bail, Context, Result};
use lazy_static::lazy_static;
use mdbook::book::Book;
use mdbook::preprocess::{Preprocessor, PreprocessorContext};
use mdbook::BookItem;
use regex::{CaptureMatches, Captures, Regex};
use std::path::{Path, PathBuf};
use std::process::Command;

#[macro_use]
extern crate log;

const MAX_LINK_NESTED_DEPTH: usize = 10;
const REL_OUTDIR: &str = "plantuml_images";
const SVG: &str = "svg";

/// A preprocessor for expanding helpers in a chapter. Supported helpers are:
///
/// - `{{# plantuml}}` - Insert a link to the rendered plantuml file
pub struct PumlPreprocessor;

impl Preprocessor for PumlPreprocessor {
    fn name(&self) -> &str {
        "plantuml-preprocessor"
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book> {
        let src_dir = ctx.root.join(&ctx.config.book.src);
        let outdir = src_dir.join(REL_OUTDIR);

        let mut plantuml_script = format!("plantuml -t{} -o {} -nometadata", SVG, outdir.display());

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                if let Some(ref chapter_path) = ch.path {
                    let base = chapter_path
                        .parent()
                        .map(|dir| src_dir.join(dir))
                        .expect("All book items have a parent");

                    let content = replace_all(
                        &ch.content,
                        &base,
                        chapter_path,
                        &outdir,
                        0,
                        &mut plantuml_script,
                    );
                    ch.content = content;
                }
            }
        });

        let status = Command::new("sh")
            .arg("-c")
            .arg(plantuml_script)
            .status()
            .with_context(|| "could not run plantuml")?;

        if !status.success() {
            bail!("could not run plantuml");
        }

        Ok(book)
    }
}

fn replace_all(
    s: &str,
    path: &Path,
    source: &Path,
    outdir: &Path,
    depth: usize,
    targets: &mut String,
) -> String {
    // When replacing one thing in a string by something with a different length,
    // the indices after that will not correspond,
    // we therefore have to store the difference to correct this
    let mut previous_end_index = 0;
    let mut replaced = String::new();

    for link in find_links(s) {
        replaced.push_str(&s[previous_end_index..link.start_index]);

        let target = path
            .join(&link.path)
            .canonicalize()
            .with_context(|| format!("{}/{}", path.display(), link.path.display()))
            .unwrap();
        targets.push_str(" \"");
        targets.push_str(&target.to_string_lossy());
        targets.push('"');

        match link.render() {
            Ok(new_content) => {
                if depth < MAX_LINK_NESTED_DEPTH {
                    let rel_path = return_relative_path(path, &link.path);
                    replaced.push_str(&replace_all(
                        &new_content,
                        &rel_path,
                        source,
                        outdir,
                        depth + 1,
                        targets,
                    ));
                } else {
                    error!(
                        "Stack depth exceeded in {}. Check for cyclic includes",
                        source.display()
                    );
                }
                previous_end_index = link.end_index;
            }
            Err(e) => {
                error!("Error updating \"{}\", {}", link.link_text, e);
                for cause in e.chain().skip(1) {
                    warn!("Caused By: {}", cause);
                }

                // This should make sure we include the raw `{{# ... }}` snippet
                // in the page content if there are any errors.
                previous_end_index = link.start_index;
            }
        }
    }

    replaced.push_str(&s[previous_end_index..]);
    replaced
}

fn return_relative_path(base: &Path, relative: &Path) -> PathBuf {
    base
        .join(relative)
        .parent()
        .expect("Included file should not be /")
        .to_path_buf()
}

#[derive(PartialEq, Debug, Clone)]
struct Link<'a> {
    start_index: usize,
    end_index: usize,
    path: PathBuf,
    link_text: &'a str,
}

impl<'a> Link<'a> {
    fn from_capture(cap: Captures<'a>) -> Option<Link<'a>> {
        let path = match cap.get(1) {
            Some(rest) => {
                let mut path_props = rest.as_str().split_whitespace();
                path_props.next().map(PathBuf::from)
            }
            _ => None,
        };

        path.and_then(|path| {
            cap.get(0).map(|mat| Link {
                start_index: mat.start(),
                end_index: mat.end(),
                path,
                link_text: mat.as_str(),
            })
        })
    }

    fn render(&self) -> Result<String> {
        let image = Path::new(
            self.path
                .file_name()
                .with_context(|| "plantuml path was not file")?,
        )
        .with_extension(SVG);

        Ok(format!(
            r#"<img src="/{}/{}" />"#,
            REL_OUTDIR,
            image.display()
        ))
    }
}

struct LinkIter<'a>(CaptureMatches<'a, 'a>);

impl<'a> Iterator for LinkIter<'a> {
    type Item = Link<'a>;
    fn next(&mut self) -> Option<Link<'a>> {
        for cap in &mut self.0 {
            if let Some(inc) = Link::from_capture(cap) {
                return Some(inc);
            }
        }
        None
    }
}

fn find_links(contents: &str) -> LinkIter<'_> {
    // lazily compute following regex
    // r"\\\{\{#plantuml\}\}|\{\{#plantuml\s*([^}]+)\}\}")?;
    lazy_static! {
        static ref RE: Regex = Regex::new(
            r"(?x)              # insignificant whitespace mode
            \\\{\{\#plantuml\}\}      # match escaped link
            |                   # or
            \{\{\s*             # link opening parens and whitespace
            \#plantuml          # link type
            \s+                 # separating whitespace
            ([^}]+)             # link target path and space separated properties
            \}\}                # link closing parens"
        )
        .unwrap();
    }
    LinkIter(RE.captures_iter(contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_links_simple_link() {
        let s = "Some random text with {{#plantuml file.puml}} and {{#plantuml ../nested/test.puml }}...";

        let res = find_links(s).collect::<Vec<_>>();
        println!("\nOUTPUT: {:?}\n", res);

        assert_eq!(
            res,
            vec![
                Link {
                    start_index: 22,
                    end_index: 45,
                    path: PathBuf::from("file.puml"),
                    link_text: "{{#plantuml file.puml}}",
                },
                Link {
                    start_index: 50,
                    end_index: 84,
                    path: PathBuf::from("../nested/test.puml"),
                    link_text: "{{#plantuml ../nested/test.puml }}",
                },
            ]
        );
    }

    #[test]
    fn replace() {
        env_logger::init();

        let root = std::env::current_dir().unwrap();

        let s = "Some random text with {{#plantuml file.puml}} and {{#plantuml ../nested/test.puml }}...";
        let path = root.join("src");
        let source = PathBuf::from("foo.md");
        let outdir = path.join("plantuml_images");

        let mut targets = String::new();

        let res = replace_all(s, &path, &source, &outdir, 0, &mut targets);

        println!("\nOUTPUT: {:?}\n", res);

        assert_eq!(
            res,
            "Some random text with <img src=\"/plantuml_images/file.svg\" /> and <img src=\"/plantuml_images/test.svg\" />..."
        );
    }
}
