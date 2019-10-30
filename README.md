# decaf-lsp

A language server implementation for [Decaf language](https://decaf-lang.gitbook.io/decaf-book/spec).

## Features

1. Workspace/document symbols
2. Symbol hovers with type information
3. Syntax diagnostics
4. Folding ranges
5. Goto definition

## Installation

Clone and install:

```
git clone https://github.com/jiegec/decaf-lsp.git
cargo install --path . --force
```

Then you can use `decaf-lsp` as a langserver.

## Editor Configuration

A VSCode extension is available at [jiegec/decaf-vscode](https://github.com/jiegec/decaf-vscode).