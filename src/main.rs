use common::Loc;
use decaf_lsp::*;
use futures::future;
use jsonrpc_core::{BoxFuture, Result};
use log::*;
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use syntax::{self, *};
use tokio;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService, Printer, Server};
use typeck;

#[derive(Debug, Default)]
struct State {
    files: HashMap<Url, FileState>,
}

#[derive(Debug, Default)]
struct FileState {
    symbols: Vec<SymbolInformation>,
    hovers: Vec<(Range, Hover)>,
}

#[derive(Debug, Default)]
struct Backend {
    state: Arc<Mutex<State>>,
}

impl State {
    fn get_file(&mut self, uri: &Url) -> &mut FileState {
        match self.files.entry(uri.clone()) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => v.insert(FileState::default()),
        }
    }
}

impl Backend {
    fn expr<'a>(
        &self,
        expr: &Expr<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        match &expr.kind {
            ExprKind::VarSel(varsel) => {
                self.varsel(&expr.loc, varsel, symbols, hovers);
            }
            ExprKind::IndexSel(indexsel) => {
                self.expr(&indexsel.arr, symbols, hovers);
                self.expr(&indexsel.idx, symbols, hovers);
            }
            ExprKind::Call(call) => {
                self.expr(&call.func, symbols, hovers);
                for arg in call.arg.iter() {
                    self.expr(&arg, symbols, hovers);
                }
            }
            ExprKind::Unary(un) => {
                self.expr(&un.r, symbols, hovers);
            }
            ExprKind::Binary(bin) => {
                self.expr(&bin.l, symbols, hovers);
                self.expr(&bin.r, symbols, hovers);
            }
            _ => {}
        }
    }

    fn varsel<'a>(
        &self,
        loc: &Loc,
        varsel: &VarSel<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        hovers.push((
            range_name(loc, varsel.name),
            Hover {
                contents: HoverContents::Scalar(MarkedString::from_markdown(format!(
                    "{}: {:?}",
                    varsel.name,
                    varsel.ty.get(),
                ))),
                range: Some(range(&loc)),
            },
        ));
        if let Some(expr) = &varsel.owner {
            self.expr(&expr, symbols, hovers);
        }
    }

    fn var<'a>(
        &self,
        var: &VarDef<'a>,
        _symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        hovers.push((
            range_name(&var.loc, var.name),
            Hover {
                contents: HoverContents::Scalar(MarkedString::from_markdown(format!(
                    "{}: {:?}",
                    var.name,
                    var.ty.get(),
                ))),
                range: Some(range(&var.loc)),
            },
        ));
    }

    fn stmt<'a>(
        &self,
        stmt: &Stmt<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        match &stmt.kind {
            StmtKind::Assign(assign) => {
                self.expr(&assign.dst, symbols, hovers);
                self.expr(&assign.src, symbols, hovers);
            }
            StmtKind::LocalVarDef(var) => {
                self.var(var, symbols, hovers);
                if let Some((_loc, expr)) = &var.init {
                    self.expr(expr, symbols, hovers);
                }
            }
            StmtKind::ExprEval(expr) => {
                self.expr(expr, symbols, hovers);
            }
            StmtKind::If(i) => {
                self.expr(&i.cond, symbols, hovers);
                self.block(&i.on_true, symbols, hovers);
                if let Some(f) = &i.on_false {
                    self.block(f, symbols, hovers);
                }
            }
            StmtKind::While(w) => {
                self.expr(&w.cond, symbols, hovers);
                self.block(&w.body, symbols, hovers);
            }
            StmtKind::For(f) => {
                self.stmt(&f.init, symbols, hovers);
                self.expr(&f.cond, symbols, hovers);
                self.stmt(&f.update, symbols, hovers);
                self.block(&f.body, symbols, hovers);
            }
            StmtKind::Return(Some(expr)) => {
                self.expr(&expr, symbols, hovers);
            }
            StmtKind::Print(exprs) => {
                for expr in exprs.iter() {
                    self.expr(&expr, symbols, hovers);
                }
            }
            StmtKind::Block(block) => {
                self.block(&block, symbols, hovers);
            }
            _ => {}
        }
    }

    fn block<'a>(
        &self,
        block: &Block<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        for stmt in block.stmt.iter() {
            self.stmt(stmt, symbols, hovers);
        }
    }

    fn field<'a>(
        &self,
        uri: Url,
        class: &ClassDef<'a>,
        field: &FieldDef<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        match field {
            syntax::FieldDef::FuncDef(func) => {
                symbols.push(SymbolInformation {
                    name: func.name.to_string(),
                    kind: SymbolKind::Method,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: range(&func.loc),
                    },
                    container_name: Some(class.name.to_string()),
                });
                hovers.push((
                    range_name(&func.loc, func.name),
                    Hover {
                        contents: HoverContents::Scalar(MarkedString::from_markdown(format!(
                            "{}: {:?}",
                            func.name,
                            syntax::ty::Ty::mk_func(func)
                        ))),
                        range: Some(range(&func.loc)),
                    },
                ));
                for param in func.param.iter() {
                    self.var(param, symbols, hovers);
                }
                self.block(&func.body, symbols, hovers);
            }
            syntax::FieldDef::VarDef(var) => {
                symbols.push(SymbolInformation {
                    name: var.name.to_string(),
                    kind: SymbolKind::Field,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: range(&var.loc),
                    },
                    container_name: Some(class.name.to_string()),
                });
                self.var(var, symbols, hovers);
            }
        }
    }

    fn class<'a>(
        &self,
        uri: Url,
        class: &ClassDef<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        let class_range = range2(&class.loc, &class.end);
        symbols.push(SymbolInformation {
            name: class.name.to_string(),
            kind: SymbolKind::Class,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: class_range,
            },
            container_name: None,
        });
        hovers.push((
            range_name(&class.loc, class.name),
            Hover {
                contents: HoverContents::Scalar(MarkedString::from_markdown(
                    class.name.to_string(),
                )),
                range: Some(class_range),
            },
        ));

        for field in class.field.iter() {
            self.field(uri.clone(), class, field, symbols, hovers);
        }
    }

    fn program<'a>(
        &self,
        uri: Url,
        program: &Program<'a>,
        symbols: &mut Vec<SymbolInformation>,
        hovers: &mut Vec<(Range, Hover)>,
    ) {
        for class in program.class.iter() {
            self.class(uri.clone(), class, symbols, hovers);
        }
    }

    fn update(&self, printer: &Printer, uri: Url, content: &str) {
        // hovers
        let mut tokens = syntax::parser::Lexer::new(content.as_bytes());
        let mut hovers = Vec::new();
        loop {
            use syntax::parser::TokenKind::*;
            let tok = tokens.next();
            if tok.ty == _Eof {
                break;
            }

            if tok.ty == Id || tok.ty == LPar || tok.ty == RPar || tok.ty == Semi {
                continue;
            }

            let range = token(&tok);
            hovers.push((
                range,
                Hover {
                    contents: HoverContents::Scalar(MarkedString::from_markdown(match tok.ty {
                        IntLit => format!("Integer Literal"),
                        StringLit => format!("String Literal"),
                        UntermString => format!("Unterminated String Literal"),
                        _ => format!("{:?}", tok.ty),
                    })),
                    range: None,
                },
            ));
        }
        let mut state = self.state.lock().unwrap();
        state.get_file(&uri).hovers = hovers;
        drop(state);

        // symbols
        match syntax::parser::work(content, &syntax::ASTAlloc::default()) {
            Ok(program) => {
                let mut diag = vec![];

                let alloc = typeck::TypeCkAlloc::default();
                match typeck::work(program, &alloc) {
                    Ok(_) => {
                        // Passes type checking
                    }
                    Err(errors) => {
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
                    }
                }

                printer.publish_diagnostics(uri.clone(), diag);

                // symbols and hover
                let mut symbols = Vec::new();
                let mut hovers = Vec::new();
                self.program(uri.clone(), program, &mut symbols, &mut hovers);
                symbols.reverse();
                let mut state = self.state.lock().unwrap();
                state.get_file(&uri).symbols = symbols;
                debug!("hovers {:?}", hovers);
                state.get_file(&uri).hovers.append(&mut hovers);
                drop(state);
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
    type DocumentSymbolFuture = BoxFuture<Option<DocumentSymbolResponse>>;

    fn initialize(&self, _: &Printer, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Full,
                )),
                workspace_symbol_provider: Some(true),
                document_symbol_provider: Some(true),
                hover_provider: Some(true),
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

    fn hover(&self, params: TextDocumentPositionParams) -> Self::HoverFuture {
        debug!("hover");
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document.uri);
        let mut result: Option<(Range, Hover)> = None;
        for (range, hover) in file.hovers.iter() {
            if range.start <= params.position && range.end >= params.position {
                result = Some(if let Some((old_range, old_hover)) = result {
                    if range.end <= old_range.end && range.start >= old_range.start {
                        (*range, hover.clone())
                    } else {
                        (old_range, old_hover)
                    }
                } else {
                    (*range, hover.clone())
                });
            }
        }
        Box::new(future::ok(result.map(|res| res.1)))
    }

    fn document_highlight(&self, _: TextDocumentPositionParams) -> Self::HighlightFuture {
        debug!("highlight");
        Box::new(future::ok(None))
    }

    fn document_symbol(&self, params: DocumentSymbolParams) -> Self::DocumentSymbolFuture {
        debug!("documentSymbol");
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document.uri);
        Box::new(future::ok(Some(DocumentSymbolResponse::Flat(
            file.symbols.clone(),
        ))))
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
