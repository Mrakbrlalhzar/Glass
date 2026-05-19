## Glass Roadmap

* Theme support: json theme definitions of colours. Supports multiple themes in ~/Library dir. Settings set’s default theme but can be overriden per file in toolbar dropdown and persisted in db. 
* glass menu should be ‘Glass’ *(note: `NSProcessInfo.setProcessName("Glass")` already runs at launch; needs verification on user's machine — may only repro when running the bare `glass` binary from a fresh shell rather than via cargo run)*
* script engine API over surface of all functionality *(note: `glass-script` crate exists as a placeholder; needs the QuickJS host + a designed API surface — what gets exposed (tabs, symbols, listing rows, write-paths?) is the hard part. Deferred.)*
* scripting setup e.g. scripts describe their function and add to menus *(note: depends on the scripting engine landing first.)*
* execute scripts *(note: depends on the scripting engine landing first.)*
* Function decompilation into pseudo C code or Java (for DEX classes)
