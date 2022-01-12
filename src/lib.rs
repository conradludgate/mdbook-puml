use aho_corasick::{AhoCorasick, AhoCorasickBuilder, FindIter, MatchKind};
use anyhow::{bail, Context, Result};
use lazy_static::lazy_static;
use mdbook::book::Book;
use mdbook::preprocess::{Preprocessor, PreprocessorContext};
use mdbook::BookItem;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

#[macro_use]
extern crate log;

const SVG: &str = "svg";

/// A preprocessor for prerendering plantuml as images
pub struct PumlPreprocessor;

impl Preprocessor for PumlPreprocessor {
    fn name(&self) -> &str {
        "plantuml-preprocessor"
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book> {
        let src_dir = ctx.root.join(&ctx.config.book.src);
        let cache_dir = src_dir.parent().unwrap().join(".plantuml_cache");
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("could not create {}", cache_dir.display()))?;

        let compiler = Compiler { cache_dir };

        book.for_each_mut(|section: &mut BookItem| {
            if let BookItem::Chapter(ref mut ch) = *section {
                let content = compiler.replace_all(&ch.content);
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
    cache_dir: PathBuf,
}

impl Compiler {
    fn compile(&self, target: Target) -> Result<Vec<u8>> {
        let filename = target.output.to_string();
        let filename = Path::new(&filename);
        let outfile = self
            .cache_dir
            .join(filename.with_extension(target.output_type));

        // check if we have it cached before running the command
        if !outfile.exists() {
            // execute plantuml cli
            let script = format!("plantuml -t{} -nometadata -pipe", target.output_type);
            let mut cmd = Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(std::fs::File::create(&outfile)?)
                .spawn()
                .with_context(|| "could not run command")?;

            cmd.stdin
                .take()
                .with_context(|| "could not open stdin")?
                .write_all(target.input.as_bytes())
                .with_context(|| "could not send data to process")?;

            let status = cmd.wait().with_context(|| "could not run plantuml")?;

            if !status.success() {
                bail!("could not run plantuml");
            }
        }

        let mut file = std::fs::File::open(outfile)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn replace_all(&self, s: &str) -> String {
        // When replacing one thing in a string by something with a different length,
        // the indices after that will not correspond,
        // we therefore have to store the difference to correct this
        let mut previous_end_index = 0;
        let mut replaced = String::new();

        for link in find_pumls(s) {
            replaced.push_str(&s[previous_end_index..link.start]);

            match link.render(self) {
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

    fn render(&self, compiler: &Compiler) -> Result<String> {
        let typ = SVG;
        let uuid = self.uuid();
        let name = find_name(self.contents);
        let bytes = compiler.compile(Target {
            output: uuid,
            input: self.contents,
            name,
            output_type: typ,
        })?;
        let output = base64::encode(bytes);

        Ok(format!(
            r#"![{}](data:image/svg+xml;base64,{})"#,
            name.unwrap_or(""),
            output
        ))
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

fn find_name(contents: &str) -> Option<&str> {
    contents
        .strip_prefix("@startuml ")
        .map(|m| match m.find('\n') {
            Some(i) => &m[..i],
            None => &m,
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

        let compiler = Compiler { cache_dir };

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
