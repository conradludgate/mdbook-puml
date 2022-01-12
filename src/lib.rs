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
use tempfile::TempDir;
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

        let compiler = Compiler {
            tmpdir: TempDir::new_in(src_dir)?,
            outdir,
        };

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                let depth = ch.path.as_ref().unwrap().components().count();
                let content = compiler.replace_all(&ch.content, depth - 1);
                ch.content = content;
            }
        });

        Ok(book)
    }
}

#[derive(Debug, PartialEq, Clone)]
struct Target<'a> {
    output: Uuid,
    input: &'a str,
    name: Option<&'a str>,
    output_type: &'static str,
}

struct Compiler {
    tmpdir: TempDir,
    outdir: PathBuf,
}

impl Compiler {
    fn compile(&self, target: Target) -> Result<()> {
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
        let input = self.tmpdir.path().join(filename.with_extension(PUML));
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
            Some(name) => Path::new(name),
            None => filename,
        };
        let output = self
            .tmpdir
            .path()
            .join(output.with_extension(target.output_type));
        std::fs::rename(&output, &outfile).with_context(|| {
            format!(
                "could not move compiled file ({}) to outdir ({})",
                output.display(),
                outfile.display()
            )
        })?;

        Ok(())
    }

    fn replace_all(&self, s: &str, depth: usize) -> String {
        // When replacing one thing in a string by something with a different length,
        // the indices after that will not correspond,
        // we therefore have to store the difference to correct this
        let mut previous_end_index = 0;
        let mut replaced = String::new();

        for link in find_pumls(s) {
            replaced.push_str(&s[previous_end_index..link.start]);

            match link.render(self, depth) {
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
}

#[derive(PartialEq, Debug, Clone)]
struct Puml<'a> {
    start: usize,
    end: usize,
    contents: &'a str,
    ignore: bool,
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

    fn render(&self, compiler: &Compiler, depth: usize) -> Result<String> {
        if self.ignore {
            return Ok(format!(
                r#"```plantuml
{}```"#,
                self.contents
            ));
        }

        let uuid = self.uuid();
        let name = find_name(self.contents);
        compiler.compile(Target {
            output: uuid,
            input: self.contents,
            name,
            output_type: SVG,
        })?;

        Ok(format!(
            r#"![{}]({}{}/{}.{})"#,
            name.unwrap_or(""),
            "../".repeat(depth), // traverse up `depth` folders
            REL_OUTDIR,          // go into the relative image outdir
            uuid,                // with the uuid as the filename
            SVG                  // and svg file extension
        ))
    }
}

struct PumlIter<'a>(&'a str, FindIter<'a, 'a, usize>);

impl<'a> Iterator for PumlIter<'a> {
    type Item = Puml<'a>;
    fn next(&mut self) -> Option<Puml<'a>> {
        let start = loop {
            let m = self.1.next()?;
            if m.pattern() != 1 {
                break m;
            }
        };

        let end = self.1.next()?;
        Some(Puml {
            start: start.start(),
            end: end.end(),
            contents: &self.0[start.end()..end.start()],
            ignore: start.pattern() == 2,
        })
    }
}

fn find_pumls(contents: &str) -> PumlIter<'_> {
    // lazily compute following regex
    // r"\\\{\{#plantuml\}\}|\{\{#plantuml\s*([^}]+)\}\}")?;
    lazy_static! {
        static ref AC: AhoCorasick = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(["```plantuml\n", "```", "```plantuml,ignore\n"]);
    }
    PumlIter(contents, AC.find_iter(contents))
}

fn find_name(contents: &str) -> Option<&str> {
    contents
        .strip_prefix("@startuml ")
        .map(|m| match m.find('\n') {
            Some(i) => &m[..i],
            None => m,
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

```plantuml,ignore
@startuml
Foo <-> Bar
@enduml
```
"#;

        let res = find_pumls(s).collect::<Vec<_>>();

        assert_eq!(
            res,
            vec![
                Puml {
                    start: 22,
                    end: 88,
                    contents: "@startuml Document Name\n\nUML <-> Document\n\n@enduml\n",
                    ignore: false,
                },
                Puml {
                    start: 125,
                    end: 174,
                    contents: "@startuml Another Doc\nFoo\n@enduml\n",
                    ignore: false,
                },
                Puml {
                    start: 176,
                    end: 228,
                    contents: "@startuml\nFoo <-> Bar\n@enduml\n",
                    ignore: true,
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
Foo <-> Bar
@enduml
```

```plantuml,ignore
@startuml
Foo <-> Bar
@enduml
```
"#;

        let tmp = TempDir::new().unwrap();
        let compiler = Compiler {
            tmpdir: TempDir::new().unwrap(),
            outdir: tmp.path().to_owned(),
        };

        let res = compiler.replace_all(s, 2);

        assert_eq!(
            res,
            r#"Some random text with
![Document Name](../../plantuml_images/bd15ddc5-f769-719d-dbb5-be1228872d69.svg)

and

```rust
let foo = "bar";
```

![](../../plantuml_images/3a1375f3-0f44-4b13-f722-de95a4661ce7.svg)

```plantuml
@startuml
Foo <-> Bar
@enduml
```
"#
        );
    }
}
