use std::fmt::Display;
use std::io::Write;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use swc_common::errors::{Handler, HANDLER};
use swc_common::input::StringInput;
use swc_common::sync::Lrc;
use swc_common::{FileName, Globals, Mark, SourceMap, GLOBALS};
use swc_ecma_transforms_base::resolver::resolver_with_mark;
use swc_ecmascript::ast::Module;
use swc_ecmascript::parser::lexer::Lexer;
use swc_ecmascript::parser::{EsConfig, Parser, Syntax};
use swc_ecmascript::visit::VisitMutWith;
use turbo_tasks_fs::FileContent;

use crate::analyzer::graph::EvalContext;
use crate::asset::AssetVc;

#[turbo_tasks::value(shared)]
pub enum ParseResult {
    Ok {
        #[trace_ignore]
        module: Module,
        #[trace_ignore]
        eval_context: EvalContext,
        #[trace_ignore]
        globals: Globals,
        #[trace_ignore]
        source_map: Arc<SourceMap>,
    },
    Unparseable,
    NotFound,
}

impl PartialEq for ParseResult {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ok { .. }, Self::Ok { .. }) => false,
            _ => core::mem::discriminant(self) == core::mem::discriminant(other),
        }
    }
}

#[derive(Clone)]
pub struct Buffer {
    buf: Arc<RwLock<Vec<u8>>>,
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            buf: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.read().unwrap().is_empty()
    }
}

impl Display for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Ok(str) = std::str::from_utf8(&self.buf.read().unwrap()) {
            let mut lines = str
                .lines()
                .map(|line| {
                    if line.len() > 300 {
                        format!("{}...{}\n", &line[..150], &line[line.len() - 150..])
                    } else {
                        format!("{}\n", line)
                    }
                })
                .collect::<Vec<_>>();
            if lines.len() > 500 {
                let (first, rem) = lines.split_at(250);
                let (_, last) = rem.split_at(rem.len() - 250);
                lines = first
                    .into_iter()
                    .chain(&["...".to_string()])
                    .chain(last.into_iter())
                    .map(|s| s.clone())
                    .collect();
            }
            let str = lines.concat();
            write!(f, "{}", str)
        } else {
            Err(std::fmt::Error)
        }
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.write().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[turbo_tasks::function]
pub async fn parse(source: AssetVc) -> Result<ParseResultVc> {
    let content = source.content();
    let fs_path = source.path().await?;
    Ok(match &*content.await? {
        FileContent::NotFound => ParseResult::NotFound.into(),
        FileContent::Content(buffer) => {
            match String::from_utf8(buffer.clone()) {
                Err(_err) => ParseResult::Unparseable.into(),
                Ok(string) => {
                    let cm: Lrc<SourceMap> = Default::default();
                    let buf = Buffer::new();
                    let handler =
                        Handler::with_emitter_writer(Box::new(buf.clone()), Some(cm.clone()));

                    let fm = cm.new_source_file(FileName::Custom(fs_path.path.clone()), string);

                    let lexer = Lexer::new(
                        Syntax::Es(EsConfig {
                            jsx: true,
                            fn_bind: true,
                            decorators: true,
                            decorators_before_export: true,
                            export_default_from: true,
                            import_assertions: true,
                            static_blocks: true,
                            private_in_object: true,
                            allow_super_outside_method: true,
                        }),
                        Default::default(),
                        StringInput::from(&*fm),
                        None,
                    );

                    let mut parser = Parser::new_from(lexer);

                    let mut has_errors = false;
                    for e in parser.take_errors() {
                        // TODO report them in a stream
                        e.into_diagnostic(&handler).emit();
                        has_errors = true
                    }

                    // TODO report them in a stream
                    if has_errors {
                        println!("{}", buf);
                        return Err(anyhow!("{}", buf));
                    }

                    match parser.parse_module() {
                        Err(e) => {
                            // TODO report in in a stream
                            e.into_diagnostic(&handler).emit();
                            return Err(anyhow!("{}", buf));
                            // ParseResult::Unparseable.into()
                        }
                        Ok(mut parsed_module) => {
                            let globals = Globals::new();
                            let eval_context = GLOBALS.set(&globals, || {
                                let top_level_mark = Mark::fresh(Mark::root());
                                HANDLER.set(&handler, || {
                                    parsed_module
                                        .visit_mut_with(&mut resolver_with_mark(top_level_mark));
                                });

                                EvalContext::new(&parsed_module, top_level_mark)
                            });

                            if !buf.is_empty() {
                                // TODO report in in a stream
                                println!("{}", buf);
                                return Err(anyhow!("{}", buf));
                            }

                            ParseResult::Ok {
                                module: parsed_module,
                                eval_context,
                                globals,
                                source_map: cm.clone(),
                            }
                            .into()
                        }
                    }
                }
            }
        }
    })
}
