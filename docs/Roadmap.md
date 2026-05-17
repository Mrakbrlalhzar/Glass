## Glass Roadmap

* Opening a 2nd __text window seems to hang at the end of ‘Disassembling…’ progress bar and not transition to the disassembly view.
* ~~Control flow graph view of disassembly~~
* ~~Control flow graph for DEX methods~~
* ~~CLI command defaulting to GUI (no ‘gui’ command needed)~~
* ~~Closing the last window terminates the command line app~~
* glass menu should be ‘Glass’ *(note: `NSProcessInfo.setProcessName("Glass")` already runs at launch; needs verification on user's machine — may only repro when running the bare `glass` binary from a fresh shell rather than via cargo run)*
* ~~Opening a file in an empty window does not create a new window but reuses this one~~
* ~~Open 2nd file, offset x,y position of 2nd window~~
* ~~In disassembly view, don’t allow cursor to select basic block rule line~~
* References search, right click in any view ‘find references’ i.e. calls to current method / function or reference to data location (hex view). *(note: needs design — new XrefResults tab kind, scope decisions DEX↔native↔data, persistence; deferred from initial roadmap pass)*
* ~~Goto address function - top bar (straight to section) - validates address as typing (red -> white) and then jump to appropriate view~~
* ~~Overview bar cursor should go to nearest function start for disassembly views rather than actual address (might not be byte aligned otherwise)~~
* ~~Package as a MacOS app in a CI job (github action)~~
* Extend search to search for instruction sequences with operands fuzzy e.g. adrp <reg>; add <reg>, <reg>, <anything> *(note: query language design first — sketch a pattern grammar + matcher prototype against an existing SymbolMap+text-section before plumbing into the cmd-F palette. Deferred.)*
* script engine API over surface of all functionality *(note: `glass-script` crate exists as a placeholder; needs the QuickJS host + a designed API surface — what gets exposed (tabs, symbols, listing rows, write-paths?) is the hard part. Deferred.)*
* scripting setup e.g. scripts describe their function and add to menus *(note: depends on the scripting engine landing first.)*
* execute scripts *(note: depends on the scripting engine landing first.)*
* In place edits (instructions & data), patch and save binary *(note: needs a writer in armv8-encode + reverse mapping from edited rows back to container offsets; also UX for an edit/undo stack and a "save patched binary" path. Deferred.)*
* Command line capabilities *(note: scope unclear — likely means non-GUI subcommands that mirror in-app operations (search, xrefs, dump-symbols). Worth designing alongside the scripting engine since they share an API surface. Deferred.)*
* MCP skills catalog over API *(note: depends on the scripting/API surface landing first — the MCP server would be a transport over the same API.)*