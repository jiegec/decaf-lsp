use common::Loc;
use decaf_lsp::*;
use jsonrpc_core::Result;
use log::*;
use serde_json::Value;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use syntax::{self, *};
use tokio;
use tower_lsp::lsp_types::request::*;
use tower_lsp::lsp_types::*;
use tower_lsp::{LanguageServer, LspService, Printer, Server};
use typeck;

#[derive(Debug, Default)]
struct State {
    files: HashMap<Url, FileState>,
}

#[derive(Debug, Default)]
struct FileState {
    content: String,
    symbols: Vec<SymbolInformation>,
    hovers: Vec<(Range, Hover)>,
    ranges: Vec<FoldingRange>,
    definitions: Vec<(Range, Range)>, // ref, def
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
    fn expr<'a>(&self, expr: &Expr<'a>, state: &mut FileState) {
        match &expr.kind {
            ExprKind::VarSel(varsel) => {
                self.varsel(&expr.loc, varsel, state);
            }
            ExprKind::IndexSel(indexsel) => {
                self.expr(&indexsel.arr, state);
                self.expr(&indexsel.idx, state);
            }
            ExprKind::Call(call) => {
                self.expr(&call.func, state);
                for arg in call.arg.iter() {
                    self.expr(&arg, state);
                }
            }
            ExprKind::Unary(un) => {
                self.expr(&un.r, state);
            }
            ExprKind::Binary(bin) => {
                self.expr(&bin.l, state);
                self.expr(&bin.r, state);
            }
            _ => {}
        }
    }

    fn varsel<'a>(&self, loc: &Loc, varsel: &VarSel<'a>, state: &mut FileState) {
        state.hovers.push((
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
            self.expr(&expr, state);
        }
        if let Some(var) = &varsel.var.get() {
            debug!("var {} {:?} {:?}", var.name, var.loc, var.ty.get());
            state.definitions.push((
                range_name(&loc, varsel.name),
                range_name(&var.loc, var.name),
            ));
        }
    }

    fn var<'a>(&self, var: &VarDef<'a>, state: &mut FileState) {
        state.hovers.push((
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

    fn stmt<'a>(&self, stmt: &Stmt<'a>, state: &mut FileState) {
        match &stmt.kind {
            StmtKind::Assign(assign) => {
                self.expr(&assign.dst, state);
                self.expr(&assign.src, state);
            }
            StmtKind::LocalVarDef(var) => {
                self.var(var, state);
                if let Some((_loc, expr)) = &var.init {
                    self.expr(expr, state);
                }
            }
            StmtKind::ExprEval(expr) => {
                self.expr(expr, state);
            }
            StmtKind::If(i) => {
                self.expr(&i.cond, state);
                self.block(&i.on_true, state);
                if let Some(f) = &i.on_false {
                    self.block(f, state);
                }
            }
            StmtKind::While(w) => {
                self.expr(&w.cond, state);
                self.block(&w.body, state);
            }
            StmtKind::For(f) => {
                self.stmt(&f.init, state);
                self.expr(&f.cond, state);
                self.stmt(&f.update, state);
                self.block(&f.body, state);
            }
            StmtKind::Return(Some(expr)) => {
                self.expr(&expr, state);
            }
            StmtKind::Print(exprs) => {
                for expr in exprs.iter() {
                    self.expr(&expr, state);
                }
            }
            StmtKind::Block(block) => {
                self.block(&block, state);
            }
            _ => {}
        }
    }

    fn block<'a>(&self, block: &Block<'a>, state: &mut FileState) {
        for stmt in block.stmt.iter() {
            self.stmt(stmt, state);
        }
    }

    fn field<'a>(
        &self,
        uri: Url,
        class: &ClassDef<'a>,
        field: &FieldDef<'a>,
        state: &mut FileState,
    ) {
        match field {
            syntax::FieldDef::FuncDef(func) => {
                state.symbols.push(SymbolInformation {
                    name: func.name.to_string(),
                    kind: SymbolKind::Method,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: range(&func.loc),
                    },
                    container_name: Some(class.name.to_string()),
                });
                state.hovers.push((
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
                    self.var(param, state);
                }
                self.block(&func.body, state);
            }
            syntax::FieldDef::VarDef(var) => {
                state.symbols.push(SymbolInformation {
                    name: var.name.to_string(),
                    kind: SymbolKind::Field,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: range(&var.loc),
                    },
                    container_name: Some(class.name.to_string()),
                });
                self.var(var, state);
            }
        }
    }

    fn class<'a>(&self, uri: Url, class: &ClassDef<'a>, state: &mut FileState) {
        let class_range = range2(&class.loc, &class.end);
        state.symbols.push(SymbolInformation {
            name: class.name.to_string(),
            kind: SymbolKind::Class,
            deprecated: None,
            location: Location {
                uri: uri.clone(),
                range: class_range,
            },
            container_name: None,
        });
        state.hovers.push((
            range_name(&class.loc, class.name),
            Hover {
                contents: HoverContents::Scalar(MarkedString::from_markdown(
                    class.name.to_string(),
                )),
                range: Some(class_range),
            },
        ));
        state.ranges.push(FoldingRange {
            start_line: (class.loc.0 - 1) as u64,
            start_character: None,
            end_line: (class.end.0 - 1) as u64,
            end_character: None,
            kind: Some(FoldingRangeKind::Region),
        });

        for field in class.field.iter() {
            self.field(uri.clone(), class, field, state);
        }
    }

    fn program<'a>(&self, uri: Url, program: &Program<'a>, state: &mut FileState) {
        for class in program.class.iter() {
            self.class(uri.clone(), class, state);
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

            if tok.ty == Id
                || tok.ty == Le
                || tok.ty == Ge
                || tok.ty == Eq
                || tok.ty == Ne
                || tok.ty == And
                || tok.ty == Add
                || tok.ty == Sub
                || tok.ty == Mul
                || tok.ty == Div
                || tok.ty == Mod
                || tok.ty == Assign
                || tok.ty == Lt
                || tok.ty == Gt
                || tok.ty == Dot
                || tok.ty == Comma
                || tok.ty == Semi
                || tok.ty == Not
                || tok.ty == LPar
                || tok.ty == RPar
                || tok.ty == LBrk
                || tok.ty == RBrk
                || tok.ty == LBrc
                || tok.ty == RBrc
                || tok.ty == Colon
            {
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
        state.get_file(&uri).content = String::from(content);
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
                                tags: None,
                            });
                        }
                    }
                }

                printer.publish_diagnostics(uri.clone(), diag, None);

                // symbols, hovers and ranges
                let mut file_state = FileState::default();
                self.program(uri.clone(), program, &mut file_state);
                file_state.symbols.reverse();
                debug!("hovers {:?}", file_state.hovers);
                debug!("def {:?}", file_state.definitions);
                let mut state = self.state.lock().unwrap();
                state.get_file(&uri).symbols = file_state.symbols;
                state.get_file(&uri).hovers.append(&mut file_state.hovers);
                state.get_file(&uri).ranges = file_state.ranges;
                state.get_file(&uri).definitions = file_state.definitions;
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
                        tags: None,
                    });
                }
                printer.publish_diagnostics(uri, diag, None);
            }
        }
    }

    fn complete(&self, _loc: Loc, name: &str) -> Vec<CompletionItem> {
        let mut res = Vec::new();
        for builtin in ["Print", "ReadInteger", "ReadLine"].iter() {
            if builtin.starts_with(name) {
                let insert_text = if *builtin == "Print" {
                    format!("{}($1)", builtin)
                } else {
                    format!("{}()", builtin)
                };
                res.push(CompletionItem {
                    label: String::from(*builtin),
                    kind: Some(CompletionItemKind::Function),
                    insert_text: Some(insert_text),
                    insert_text_format: Some(InsertTextFormat::Snippet),
                    ..CompletionItem::default()
                });
            }
        }
        res
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    fn initialize(&self, _: &Printer, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Full,
                )),
                workspace_symbol_provider: Some(true),
                document_symbol_provider: Some(true),
                hover_provider: Some(true),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                definition_provider: Some(true),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: None,
                    trigger_characters: Some(vec![String::from("R"), String::from("P")]),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn shutdown(&self) -> Result<()> {
        debug!("shutdown");
        Ok(())
    }

    async fn symbol(&self, _: WorkspaceSymbolParams) -> Result<Option<Vec<SymbolInformation>>> {
        debug!("symbol");
        let state = self.state.lock().unwrap();
        let mut symbols = Vec::new();
        for (_, file) in state.files.iter() {
            symbols.append(&mut file.symbols.clone());
        }
        Ok(Some(symbols))
    }

    /*
    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<FoldingRange>> {
        debug!("folding");
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document.uri);
        Ok(Some(file.ranges.clone()))
    }
    */

    async fn execute_command(&self, _: &Printer, _: ExecuteCommandParams) -> Result<Option<Value>> {
        debug!("exec");
        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        debug!("complete");
        let position = params.text_document_position.position;
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document_position.text_document.uri);
        let lines: Vec<&str> = file.content.split("\n").collect();
        if let Some(line) = lines.get(position.line as usize) {
            let part = &line[..position.character as usize];
            if let Some(name) = part.rmatches(char::is_alphabetic).next() {
                debug!("{}", name);
                let loc = Loc(position.line as u32 + 1, position.character as u32 + 1);
                return Ok(Some(CompletionResponse::Array(self.complete(loc, name))));
            }
        }
        Ok(None)
    }

    async fn hover(&self, params: TextDocumentPositionParams) -> Result<Option<Hover>> {
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
        Ok(result.map(|res| res.1))
    }

    async fn document_highlight(
        &self,
        _: TextDocumentPositionParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        debug!("highlight");
        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        debug!("documentSymbol");
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document.uri);
        Ok(Some(DocumentSymbolResponse::Flat(file.symbols.clone())))
    }

    async fn goto_definition(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        debug!("definition");
        let mut state = self.state.lock().unwrap();
        let file = state.get_file(&params.text_document.uri);
        let mut result: Option<(Range, Range)> = None;
        for (range, def) in file.definitions.iter() {
            if range.start <= params.position && range.end >= params.position {
                result = Some(if let Some((old_range, old_hover)) = result {
                    if range.end <= old_range.end && range.start >= old_range.start {
                        (*range, def.clone())
                    } else {
                        (old_range, old_hover)
                    }
                } else {
                    (*range, def.clone())
                });
            }
        }
        Ok(result.map(|res| {
            GotoDefinitionResponse::Scalar(Location {
                uri: params.text_document.uri.clone(),
                range: res.1,
            })
        }))
    }

    async fn goto_declaration(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition(params).await
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
        printer.publish_diagnostics(params.text_document.uri, vec![], None);
    }
}

#[tokio::main]
async fn main() {
    simple_logging::log_to_file(".decaf-lsp.log", LevelFilter::Debug).unwrap();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(Backend::default());
    Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service)
        .await;
}
