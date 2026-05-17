//! Tiny hand-rolled lexer for smali source lines.
//!
//! Smali's grammar is small enough that a per-character pass is the
//! right level. Falls back to a single `Plain` chunk for anything we
//! don't recognise so unknown syntax still renders.

use glass_arch_arm64::{Chunk, ChunkKind};

/// Tokenise a single line of smali into coloured chunks.
pub fn tokenize_smali_line(line: &str) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    let push = |out: &mut Vec<Chunk>, text: String, kind: ChunkKind| {
        if !text.is_empty() {
            out.push(Chunk { text, kind, target: None, target_text: None });
        }
    };

    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == ' ' || c == '\t' {
            let start = i;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Plain);
            continue;
        }
        if c == '#' {
            push(&mut out, line[i..].to_string(), ChunkKind::Comment);
            break;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::String);
            continue;
        }
        if c == '.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Directive);
            continue;
        }
        if c == ':' && i + 1 < bytes.len() && smali_ident_byte(bytes[i + 1]) {
            let start = i;
            i += 1;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Label);
            continue;
        }
        if (c == 'L' || c == '[')
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'L' || bytes[i + 1] == b'[')
            && !preceded_by_ident_char(bytes, i)
        {
            let start = i;
            let mut j = i;
            while j < bytes.len() && bytes[j] == b'[' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'L' {
                j += 1;
                while j < bytes.len() && bytes[j] != b';' {
                    j += 1;
                }
                if j < bytes.len() {
                    j += 1;
                }
            } else if j < bytes.len() && b"VZBSCIJFD".contains(&bytes[j]) {
                j += 1;
            } else {
                push(&mut out, c.to_string(), ChunkKind::Plain);
                i += 1;
                continue;
            }
            let type_text = line[start..j].to_string();
            push(&mut out, type_text.clone(), ChunkKind::Type);
            i = j;
            if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'>' {
                let arrow_start = i;
                let mut k = i + 2;
                let name_start = k;
                while k < bytes.len() && smali_ident_byte(bytes[k]) {
                    k += 1;
                }
                let name_end = k;
                if name_end > name_start && k < bytes.len() && bytes[k] == b'(' {
                    while k < bytes.len() && bytes[k] != b')' {
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b')' {
                        k += 1;
                    }
                    while k < bytes.len() && bytes[k] == b'[' {
                        k += 1;
                    }
                    if k < bytes.len() {
                        if bytes[k] == b'L' {
                            k += 1;
                            while k < bytes.len() && bytes[k] != b';' {
                                k += 1;
                            }
                            if k < bytes.len() {
                                k += 1;
                            }
                        } else if b"VZBSCIJFD".contains(&bytes[k]) {
                            k += 1;
                        }
                    }
                    push(&mut out, "->".to_string(), ChunkKind::Punct);
                    let method_body = line[name_start..k].to_string();
                    let full_ref = format!("{type_text}->{method_body}");
                    out.push(Chunk {
                        text: method_body,
                        kind: ChunkKind::MethodName,
                        target: None,
                        target_text: Some(full_ref),
                    });
                    i = k;
                    continue;
                }
                let _ = arrow_start;
            }
            continue;
        }
        if matches!(c, 'V' | 'Z' | 'B' | 'S' | 'C' | 'I' | 'J' | 'F' | 'D')
            && preceded_by_byte(bytes, i, b')')
        {
            push(&mut out, c.to_string(), ChunkKind::Type);
            i += 1;
            continue;
        }
        if (c == 'v' || c == 'p')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && !preceded_by_ident_char(bytes, i)
        {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Register);
            continue;
        }
        if c.is_ascii_digit()
            || (c == '-'
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit())
        {
            let start = i;
            if c == '-' {
                i += 1;
            }
            if i + 1 < bytes.len() && bytes[i] == b'0' && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                i += 2;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
            }
            if i < bytes.len() && b"LlFfDdSstT".contains(&bytes[i]) {
                i += 1;
            }
            push(&mut out, line[start..i].to_string(), ChunkKind::Immediate);
            continue;
        }
        if smali_ident_start(c as u8) {
            let start = i;
            while i < bytes.len() && smali_ident_byte(bytes[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let kind = classify_smali_word(word, out.last().map(|c| c.text.as_str()));
            push(&mut out, word.to_string(), kind);
            continue;
        }
        let start = i;
        while i < bytes.len() && is_smali_punct(bytes[i]) {
            i += 1;
        }
        if i == start {
            let step = utf8_char_byte_len(bytes[i]);
            let end = (i + step).min(bytes.len());
            push(&mut out, line[i..end].to_string(), ChunkKind::Plain);
            i = end;
        } else {
            push(&mut out, line[start..i].to_string(), ChunkKind::Punct);
        }
    }

    out
}

/// If `chunk_text` is a class JNI (`Lcom/example/Foo;`, possibly
/// preceded by array markers `[[Lcom/...;`) return the bare class JNI
/// without the leading `[`s.
pub fn extract_class_jni(chunk_text: &str) -> Option<&str> {
    let trimmed = chunk_text.trim_start_matches('[');
    if trimmed.starts_with('L') && trimmed.ends_with(';') && trimmed.len() > 2 {
        Some(trimmed)
    } else {
        None
    }
}

fn smali_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'<' | b'$')
}

fn smali_ident_byte(b: u8) -> bool {
    smali_ident_start(b) || b.is_ascii_digit() || matches!(b, b'-' | b'/' | b'>')
}

fn is_smali_punct(b: u8) -> bool {
    matches!(
        b,
        b',' | b'{' | b'}' | b'(' | b')' | b';' | b'=' | b'!' | b'?'
        | b'-' | b'>' | b'/' | b'+' | b'*' | b'&' | b'|' | b'^' | b'~'
        | b'.' | b':' | b'@'
    )
}

fn preceded_by_ident_char(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = bytes[i - 1];
    prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$'
}

fn preceded_by_byte(bytes: &[u8], i: usize, b: u8) -> bool {
    i > 0 && bytes[i - 1] == b
}

fn utf8_char_byte_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xc0 {
        1
    } else if b < 0xe0 {
        2
    } else if b < 0xf0 {
        3
    } else {
        4
    }
}

fn classify_smali_word(word: &str, prev_text: Option<&str>) -> ChunkKind {
    use ChunkKind as K;
    match word {
        "invoke-virtual" | "invoke-direct" | "invoke-static" | "invoke-super"
        | "invoke-interface" | "invoke-virtual/range" | "invoke-direct/range"
        | "invoke-static/range" | "invoke-super/range" | "invoke-interface/range"
        | "invoke-polymorphic" | "invoke-polymorphic/range"
        | "invoke-custom" | "invoke-custom/range"
        | "move" | "move/from16" | "move/16" | "move-wide" | "move-wide/from16"
        | "move-wide/16" | "move-object" | "move-object/from16"
        | "move-object/16" | "move-result" | "move-result-wide"
        | "move-result-object" | "move-exception"
        | "return" | "return-void" | "return-wide" | "return-object"
        | "const" | "const/4" | "const/16" | "const/high16" | "const-wide"
        | "const-wide/16" | "const-wide/32" | "const-wide/high16"
        | "const-string" | "const-string/jumbo" | "const-class"
        | "monitor-enter" | "monitor-exit" | "check-cast" | "instance-of"
        | "array-length" | "new-instance" | "new-array" | "filled-new-array"
        | "filled-new-array/range" | "fill-array-data" | "throw"
        | "goto" | "goto/16" | "goto/32"
        | "packed-switch" | "sparse-switch"
        | "cmpl-float" | "cmpg-float" | "cmpl-double" | "cmpg-double" | "cmp-long"
        | "if-eq" | "if-ne" | "if-lt" | "if-ge" | "if-gt" | "if-le"
        | "if-eqz" | "if-nez" | "if-ltz" | "if-gez" | "if-gtz" | "if-lez"
        | "nop" => K::Mnemonic,

        "public" | "private" | "protected" | "static" | "final" | "abstract"
        | "native" | "synchronized" | "transient" | "volatile" | "synthetic"
        | "bridge" | "varargs" | "constructor" | "interface" | "enum"
        | "annotation" | "declared-synchronized" | "strict" | "strictfp"
        | "fpstrict" => K::Modifier,

        _ => {
            let mnemonic_prefixes: &[&str] = &[
                "iget", "iput", "sget", "sput", "aget", "aput",
                "add-", "sub-", "mul-", "div-", "rem-",
                "and-", "or-", "xor-", "shl-", "shr-", "ushr-",
                "neg-", "not-",
                "int-to-", "long-to-", "float-to-", "double-to-",
            ];
            if mnemonic_prefixes.iter().any(|p| word.starts_with(p)) {
                return K::Mnemonic;
            }
            let _ = prev_text;
            K::Plain
        }
    }
}
