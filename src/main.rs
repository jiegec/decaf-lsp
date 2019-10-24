use decaf_lsp::*;
use futures::future;
use jsonrpc_core::{BoxFuture, Result};
use log::*;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use syntax;
use tokio;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService, Printer, Server};

#[derive(Debug, Default)]
struct State {
    files: HashMap<Url, FileState>,
}

#[derive(Debug, Default)]
struct FileState {
    symbols: Vec<SymbolInformation>,
}

#[derive(Debug, Default)]
struct Backend {
    state: Arc<Mutex<State>>,
}

impl Backend {
    fn update(&self, printer: &Printer, uri: Url, content: &str) {
        // symbols
        match syntax::parser::work(content, &syntax::ASTAlloc::default()) {
            Ok(program) => {
                let mut symbols = Vec::new();
                for class in program.class.iter() {
                    symbols.push(SymbolInformation {
                        name: class.name.to_string(),
                        kind: SymbolKind::Class,
                        deprecated: None,
                        location: Location {
                            uri: uri.clone(),
                            range: range2(&class.loc, &class.end),
                        },
                        container_name: None,
                    });

                    for field in class.field.iter() {
                        match field {
                            syntax::FieldDef::FuncDef(func) => symbols.push(SymbolInformation {
                                name: func.name.to_string(),
                                kind: SymbolKind::Method,
                                deprecated: None,
                                location: Location {
                                    uri: uri.clone(),
                                    range: range(&func.loc),
                                },
                                container_name: Some(class.name.to_string()),
                            }),
                            syntax::FieldDef::VarDef(var) => symbols.push(SymbolInformation {
                                name: var.name.to_string(),
                                kind: SymbolKind::Field,
                                deprecated: None,
                                location: Location {
                                    uri: uri.clone(),
                                    range: range(&var.loc),
                                },
                                container_name: Some(class.name.to_string()),
                            }),
                        }
                    }
                }
                let mut state = self.state.lock().unwrap();
                if let Some(state) = state.files.get_mut(&uri) {
                    state.symbols = symbols;
                }
                printer.publish_diagnostics(uri, vec![]);
            }
            Err(errors) => {
                let mut diag = Vec::new();
                for err in errors.0.iter() {
                    diag.push(Diagnostic {
                        range: range(&err.0),
                        severity: None,
                        code: None,
                        source: None,
                        message: format!("{:?}", err.1),
                        related_information: None,
                    });
                }
                printer.publish_diagnostics(uri, diag);
            }
        }
    }
}

impl LanguageServer for Backend {
    type ShutdownFuture = BoxFuture<()>;
    type SymbolFuture = BoxFuture<Option<Vec<SymbolInformation>>>;
    type ExecuteFuture = BoxFuture<Option<Value>>;
    type CompletionFuture = BoxFuture<Option<CompletionResponse>>;
    type HoverFuture = BoxFuture<Option<Hover>>;
    type HighlightFuture = BoxFuture<Option<Vec<DocumentHighlight>>>;

    fn initialize(&self, _: &Printer, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Full,
                )),
                workspace_symbol_provider: Some(true),
                ..ServerCapabilities::default()
            },
        })
    }

    fn shutdown(&self) -> Self::ShutdownFuture {
        debug!("shutdown");
        Box::new(future::ok(()))
    }

    fn symbol(&self, _: WorkspaceSymbolParams) -> Self::SymbolFuture {
        debug!("symbol");
        let state = self.state.lock().unwrap();
        let mut symbols = Vec::new();
        for (_, file) in state.files.iter() {
            symbols.append(&mut file.symbols.clone());
        }
        Box::new(future::ok(Some(symbols.clone())))
    }

    fn execute_command(&self, _: &Printer, _: ExecuteCommandParams) -> Self::ExecuteFuture {
        debug!("exec");
        Box::new(future::ok(None))
    }

    fn completion(&self, _: CompletionParams) -> Self::CompletionFuture {
        debug!("complete");
        Box::new(future::ok(None))
    }

    fn hover(&self, _param: TextDocumentPositionParams) -> Self::HoverFuture {
        debug!("hover");
        Box::new(future::ok(None))
    }

    fn document_highlight(&self, _: TextDocumentPositionParams) -> Self::HighlightFuture {
        debug!("highlight");
        Box::new(future::ok(None))
    }

    fn did_open(&self, printer: &Printer, params: DidOpenTextDocumentParams) {
        debug!("didOpen");
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if let Ok(content) = fs::read_to_string(path) {
                self.update(printer, uri, &content);
            }
        }
    }

    fn did_change(&self, printer: &Printer, params: DidChangeTextDocumentParams) {
        debug!("didChange");
        let uri = params.text_document.uri;
        self.update(printer, uri, &params.content_changes[0].text);
    }

    fn did_close(&self, printer: &Printer, params: DidCloseTextDocumentParams) {
        debug!("didClose");
        let mut state = self.state.lock().unwrap();
        if let Some(state) = state.files.get_mut(&params.text_document.uri) {
            state.symbols.clear();
        }
        printer.publish_diagnostics(params.text_document.uri, vec![]);
    }
}

fn main() {
    simple_logging::log_to_file(".decaf-lsp.log", LevelFilter::Debug).unwrap();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(Backend::default());
    let handle = service.close_handle();
    let server = Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service);

    tokio::run(handle.run_until_exit(server));
}
