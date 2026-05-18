## Glass Roadmap

* glass menu should be ‘Glass’ *(note: `NSProcessInfo.setProcessName("Glass")` already runs at launch; needs verification on user's machine — may only repro when running the bare `glass` binary from a fresh shell rather than via cargo run)*
* Extend search to search for instruction sequences with operands fuzzy e.g. adrp <reg>; add <reg>, <reg>, <anything> *(note: query language design first — sketch a pattern grammar + matcher prototype against an existing SymbolMap+text-section before plumbing into the cmd-F palette. Deferred.)*
* script engine API over surface of all functionality *(note: `glass-script` crate exists as a placeholder; needs the QuickJS host + a designed API surface — what gets exposed (tabs, symbols, listing rows, write-paths?) is the hard part. Deferred.)*
* scripting setup e.g. scripts describe their function and add to menus *(note: depends on the scripting engine landing first.)*
* execute scripts *(note: depends on the scripting engine landing first.)*
* In place edits (instructions & data), patch and save binary *(note: needs a writer in armv8-encode + reverse mapping from edited rows back to container offsets; also UX for an edit/undo stack and a "save patched binary" path. Deferred.)*
* Function decompilation into pseudo C code or Java (for DEX classes)
