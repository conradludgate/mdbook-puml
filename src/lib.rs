use aho_corasick::{AhoCorasick, AhoCorasickBuilder, FindIter, MatchKind};
use anyhow::{bail, Context, Result};
use lazy_static::lazy_static;
use mdbook::book::Book;
use mdbook::preprocess::{Preprocessor, PreprocessorContext};
use mdbook::BookItem;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;
use uuid::Uuid;

#[macro_use]
extern crate log;

const REL_OUTDIR: &str = "plantuml_images";
const SVG: &str = "svg";
const PUML: &str = "puml";

/// A preprocessor for prerendering plantuml as images
pub struct PumlPreprocessor;

impl Preprocessor for PumlPreprocessor {
    fn name(&self) -> &str {
        "plantuml-preprocessor"
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book> {
        let src_dir = ctx.root.join(&ctx.config.book.src);
        let outdir = src_dir.join(REL_OUTDIR);
        std::fs::create_dir_all(&outdir)
            .with_context(|| format!("could not create {}", outdir.display()))?;

        let mut targets = vec![];

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                let content = replace_all(&ch.content, &mut targets);
                ch.content = content;
            }
        });

        let compiler = Compiler {
            tmpdir: tempdir()?.into_path(),
            outdir,
        };

        targets
            .iter()
            .try_for_each(|target| compiler.compile(target))?;

        Ok(book)
    }
}

#[derive(Debug, PartialEq, Clone)]
struct Target {
    output: Uuid,
    input: String,
    name: Option<String>,
    output_type: &'static str,
}

struct Compiler {
    tmpdir: PathBuf,
    outdir: PathBuf,
}

impl Compiler {
    fn compile(&self, target: &Target) -> Result<()> {
        let filename = target.output.to_string();
        let filename = Path::new(&filename);
        let outfile = self
            .outdir
            .join(filename.with_extension(target.output_type));

        // check if we have it cached
        if outfile.exists() {
            info!("{} exists. returning early", target.output);
            return Ok(());
        }

        // write the puml contents to a tmp file
        let input = self.tmpdir.join(filename.with_extension(PUML));
        std::fs::write(&input, &target.input).with_context(|| "could not create tmp puml file")?;

        // execute plantuml cli
        let script = format!(
            "plantuml -t{} -nometadata {}",
            target.output_type,
            input.display(),
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(script)
            .status()
            .with_context(|| "could not run plantuml")?;

        if !status.success() {
            bail!("could not run plantuml");
        }

        // move the compiled file to the outdir
        let output = match &target.name {
            Some(name) => Path::new(name.as_str()),
            None => filename,
        };
        let output = self.tmpdir.join(output.with_extension(target.output_type));
        std::fs::rename(output, outfile)
            .with_context(|| "could not move compiled file to ourdir")?;

        Ok(())
    }
}

fn replace_all(s: &str, targets: &mut Vec<Target>) -> String {
    // When replacing one thing in a string by something with a different length,
    // the indices after that will not correspond,
    // we therefore have to store the difference to correct this
    let mut previous_end_index = 0;
    let mut replaced = String::new();

    for link in find_pumls(s) {
        replaced.push_str(&s[previous_end_index..link.start]);

        match link.render(targets) {
            Ok(new_content) => {
                replaced.push_str(&new_content);
                previous_end_index = link.end;
            }
            Err(e) => {
                error!("Error updating \"{}\", {}", link.contents, e);
                for cause in e.chain().skip(1) {
                    warn!("Caused By: {}", cause);
                }

                // This should make sure we include the raw `{{# ... }}` snippet
                // in the page content if there are any errors.
                previous_end_index = link.start;
            }
        }
    }

    replaced.push_str(&s[previous_end_index..]);
    replaced
}

#[derive(PartialEq, Debug, Clone)]
struct Puml<'a> {
    start: usize,
    end: usize,
    contents: &'a str,
}

impl<'a> Puml<'a> {
    fn uuid(&self) -> Uuid {
        let mut hasher = DefaultHasher::new();
        hasher.write(self.contents.as_bytes());

        let lhs = hasher.finish() as u128;
        hasher.write_u8(0);
        let rhs = hasher.finish() as u128;
        Uuid::from_u128(lhs << 64 | rhs)
    }

    fn render(&self, targets: &mut Vec<Target>) -> Result<String> {
        let uuid = self.uuid();
        targets.push(Target {
            output: uuid,
            input: self.contents.to_owned(),
            name: find_name(self.contents),
            output_type: SVG,
        });

        Ok(format!(r#"<img src="/{}/{}.{}" />"#, REL_OUTDIR, uuid, SVG))
    }
}

struct PumlIter<'a>(&'a str, FindIter<'a, 'a, usize>);

impl<'a> Iterator for PumlIter<'a> {
    type Item = Puml<'a>;
    fn next(&mut self) -> Option<Puml<'a>> {
        let start = loop {
            let m = self.1.next()?;
            if m.pattern() == 0 {
                break m;
            }
        };

        let end = self.1.next()?;
        Some(Puml {
            start: start.start(),
            end: end.end(),
            contents: &self.0[start.end()..end.start()],
        })
    }
}

fn find_pumls(contents: &str) -> PumlIter<'_> {
    // lazily compute following regex
    // r"\\\{\{#plantuml\}\}|\{\{#plantuml\s*([^}]+)\}\}")?;
    lazy_static! {
        static ref AC: AhoCorasick = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(["```plantuml\n", "```"]);
    }
    PumlIter(contents, AC.find_iter(contents))
}

fn find_name(contents: &str) -> Option<String> {
    contents.strip_prefix("@startuml ").map(|m| {
        match m.find('\n') {
            Some(i) => m[..i].to_owned(),
            None => m.to_owned(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_plantuml() {
        let s = r#"Some random text with
```plantuml
@startuml Document Name

UML <-> Document

@enduml
```

and

```rust
let foo = "bar";
```

```plantuml
@startuml Another Doc
Foo
@enduml
```
..."#;

        let res = find_pumls(s).collect::<Vec<_>>();

        assert_eq!(
            res,
            vec![
                Puml {
                    start: 22,
                    end: 88,
                    contents: "@startuml Document Name\n\nUML <-> Document\n\n@enduml\n",
                },
                Puml {
                    start: 125,
                    end: 174,
                    contents: "@startuml Another Doc\nFoo\n@enduml\n",
                },
            ]
        );
    }

    #[test]
    fn replace() {
        env_logger::init();

        let s = r#"Some random text with
```plantuml
@startuml Document Name

UML <-> Document

@enduml
```

and

```rust
let foo = "bar";
```

```plantuml
@startuml
Foo
@enduml
```
..."#;

        let mut targets = Vec::new();

        let res = replace_all(s, &mut targets);

        assert_eq!(
            res,
            r#"Some random text with
<img src="/plantuml_images/bd15ddc5-f769-719d-dbb5-be1228872d69.svg" />

and

```rust
let foo = "bar";
```

<img src="/plantuml_images/8ce939d4-4d6b-5966-2d76-7b57c6c9b157.svg" />
..."#
        );

        assert_eq!(
            targets,
            vec![
                Target {
                    output: Uuid::from_u128(0x_bd15ddc5_f769_719d_dbb5_be1228872d69),
                    input: "@startuml Document Name\n\nUML <-> Document\n\n@enduml\n".to_owned(),
                    name: Some("Document Name".to_owned()),
                    output_type: SVG
                },
                Target {
                    output: Uuid::from_u128(0x_8ce939d4_4d6b_5966_2d76_7b57c6c9b157),
                    input: "@startuml\nFoo\n@enduml\n".to_owned(),
                    name: None,
                    output_type: SVG
                }
            ]
        )
    }
}
