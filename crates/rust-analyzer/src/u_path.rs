/*
debug:
    add in .vimrc
        \       'cmdline': [ 'u-analyzer', '--no-log-buffering', '--log-file', '/tmp/u' ],

    touch /tmp/u
    tail -f /tmp/u

mainly changed files:
    - crates/rust-analyzer/src/caps.rs
    - crates/rust-analyzer/src/handlers.rs
    - crates/rust-analyzer/src/main_loop.rs
*/

use crate::global_state::{url_to_file_id, GlobalState, GlobalStateSnapshot};
use ide::FileId;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Position, SignatureHelp,
    TextDocumentPositionParams, Url,
};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::sync::Arc;
use u::SpanPair;

/** map *.u file url to corresponding *.rs file url
*/
pub(crate) fn u_to_rs_url(root_path: &vfs::AbsPathBuf, u_uri: &mut Url) -> String {
    let r = String::new();
    let root = match root_path.parent() {
        Some(root) => match root.as_os_str().to_str() {
            Some(a) => a,
            None => return r,
        },
        None => return r,
    };
    let u_str = match u_uri.path().strip_prefix(root) {
        Some(a) => a,
        None => return r,
    };
    if u_str.len() <= 2 {
        return r;
    }
    let u_str = &u_str[0..u_str.len() - 2];
    let r = u_str[1..].to_string();
    let p = &format!("{}/.u{}.rs", &root, u_str);
    u_uri.set_path(p);

    r
}

/** compile .u file to .rs, and generate Vec<SpanPair>
*/
pub(crate) fn u_compile_to_rust(text: &mut String, url_path: &str) -> Vec<SpanPair> {
    return compile_to_rust(text, url_path).unwrap_or(Vec::new());

    fn compile_to_rust(
        text: &mut String,
        url_path: &str,
    ) -> Result<Vec<SpanPair>, Box<dyn std::error::Error>> {
        let buf: Vec<_> = url_path.split('/').collect();
        if buf.len() < 3 {
            tracing::error!("{:?}", buf);
            let p = format!("{:?} not supported", url_path);
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, p)));
        }

        let file_name = buf.last().unwrap();

        let is_main = if buf.len() == 3 {
            true
        } else if *file_name == "mod" {
            true
        } else {
            match buf[0] {
                "benches" | "bin" | "examples" | "tests" => {
                    // bin/a.rs or bin/a/main.rs -> true
                    // bin/a/src/a.rs -> false
                    buf.len() == 4 || buf.len() == 5
                }
                _ => false,
            }
        };

        let new_text = std::mem::take(text);
        let data = u::about_mod(new_text, *file_name == "mod");

        let mut l = u::Lex::new();
        l.lex(data)?;
        // check the first case that the token after . and .. is not an Identifier or integer or await,
        // insert an empty Identifier after . or ..
        if let Some(index) = l.tokens.windows(2).position(|a| {
            matches!(a[0].code, u::TokenCode::Op(u::Op::Dot | u::Op::DotDot))
                && !matches!(
                    a[1].code,
                    u::TokenCode::Identifier(_)
                        | u::TokenCode::Literal(_)
                        | u::TokenCode::Keyword(u::Keyword::Await)
                )
        }) {
            let token = &l.tokens[index];
            let span = u::Span {
                width: 0,
                line: token.span.line,
                column: token.span.column + token.span.width,
            };
            l.tokens.insert(
                index + 1,
                u::Token {
                    code: u::TokenCode::Identifier(u::Identifier::new(
                        String::new(),
                        span.line,
                        span.column,
                    )),
                    span,
                },
            );
        }

        let mut p = u::Parse::new(l);
        p.set_magic();
        if let Err(err) = p.parse() {
            tracing::error!("{:?}", err);
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "parse error")));
        }

        let mut f = p.to_rust(is_main)?;
        let span_pairs = f.span_pairs_move();
        *text = f.buf();
        // tracing::error!("{}", text);
        Ok(span_pairs)
    }
}

pub(crate) fn u_save_span_pairs(this: &mut GlobalState, url: &Url, pairs: Vec<SpanPair>) {
    match url_to_file_id(&this.vfs.read().0, &url) {
        Ok(file_id) => {
            this.u.add(file_id, pairs);
        }
        Err(err) => {
            tracing::error!("File in u_save_span_pairs not found in VFS: {}", err);
        }
    }
}

pub(crate) fn u_to_rs_position(
    snap: &GlobalStateSnapshot,
    params: &mut TextDocumentPositionParams,
) {
    // tracing::error!("{:?}", params.position);
    let file_id = match snap.url_to_file_id(&params.text_document.uri) {
        Ok(file_id) => file_id,
        Err(err) => {
            tracing::error!("File in u_to_rs_position not found in VFS: {}", err);
            return;
        }
    };
    let pairs = match snap.u.span_pairs.get(&file_id) {
        Some(a) => a,
        None => {
            tracing::error!("{:?} not found in snap", file_id);
            return;
        }
    };
    let Position { line, character } = params.position;
    let line = line as usize;
    let column = character as usize;
    let u_pair = match pairs.iter().find(|a| {
        a.u.line - 1 == line && a.u.column - 1 <= column && column <= a.u.column - 1 + a.u.width
    }) {
        Some(a) => a,
        None => {
            tracing::error!("no pairs match in u_to_rs_position");
            return;
        }
    };
    let delta = u_pair.rust.width;
    params.position.line = (u_pair.rust.line - 1) as u32;
    params.position.character = (u_pair.rust.column - 1 + delta) as u32;
    // tracing::error!("{:?}", u_pair);
    // tracing::error!("{:?}", params.position);
}

pub(crate) fn u_transform_completion_items(
    snap: &GlobalStateSnapshot,
    url: &Url,
    items: &mut Vec<CompletionItem>,
) {
    if items.is_empty() {
        return;
    }

    let file_id = match snap.url_to_file_id(url) {
        Ok(file_id) => file_id,
        Err(err) => {
            tracing::error!("File in u_to_rs_position not found in VFS: {}", err);
            return;
        }
    };
    let pairs = match snap.u.span_pairs.get(&file_id) {
        Some(a) => a,
        None => {
            tracing::error!("{:?} not found in snap", file_id);
            return;
        }
    };

    let mut m: BTreeMap<Position, Position> = BTreeMap::new();
    for item in items {
        match &mut item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => {
                transform_position(&mut m, &mut edit.range.start, &pairs, true);
                transform_position(&mut m, &mut edit.range.end, &pairs, false);

                transform_text(&mut edit.new_text, item.kind);
            }
            Some(CompletionTextEdit::InsertAndReplace(_)) => {
                // NOTE it seems this is not used
                // tracing::error!("insert {:#?}", insert);
            }
            None => {}
        }
        transform_text(&mut item.label, item.kind);
        if let Some(ref mut text) = item.detail {
            if let Some(CompletionItemKind::METHOD) = item.kind {
                transform_method(text);
            }
            transform_text(text, item.kind);
        }
        if let Some(ref mut text) = item.filter_text {
            transform_text(text, item.kind);
        }
    }

    // from rust_pos to u_pos
    fn transform_position(
        m: &mut BTreeMap<Position, Position>,
        pos: &mut Position,
        pairs: &[SpanPair],
        is_start: bool,
    ) {
        let Position { line, character } = *pos;
        let line = line as usize;
        let column = character as usize;

        *pos = *m.entry(*pos).or_insert_with(|| {
            let u_pair = match pairs.iter().find(|a| {
                // NOTE because of the added empty identifier after . or ..
                // region positions are processed differently
                a.rust.line - 1 == line
                    && a.rust.column - 1 <= column
                    && ((is_start && column < a.rust.column - 1 + a.rust.width
                        || a.rust.width == 0)
                        || (!is_start && column <= a.rust.column - 1 + a.rust.width))
            }) {
                Some(a) => a,
                None => {
                    tracing::error!("no pairs match in transform_position");
                    return *pos;
                }
            };
            Position { line: (u_pair.u.line - 1) as u32, character: (u_pair.u.column - 1) as u32 }
        });
    }

    fn transform_text(text: &mut String, kind: Option<CompletionItemKind>) {
        if text.is_empty() {
            return;
        }

        // tokenize text
        let mut new_text = String::new();
        let chars: Vec<_> = text.chars().collect();
        let total_len = chars.len();
        let mut i = 0;
        while i < total_len {
            let ch = chars[i];
            if ch.is_alphabetic() {
                let start_index = i;
                let mut end_index = i;
                i += 1;
                while i < total_len {
                    let new_ch = chars[i];
                    if !(new_ch.is_alphanumeric() || new_ch == '_') {
                        end_index = i;
                        break;
                    }
                    i += 1;
                }
                if i == total_len {
                    end_index = total_len;
                }
                let mut ident = chars[start_index..end_index].iter().collect();
                transform_identifier(&mut ident, kind);
                new_text.push_str(&ident);
                continue;
            }

            if ch == '-' {
                if i + 1 < total_len {
                    if chars[i + 1] == '>' {
                        if new_text.chars().last() == Some(' ') {
                            new_text.pop();
                        }
                        i += 2;
                        continue;
                    }
                }
            }
            if ch == ':' {
                if i + 1 < total_len {
                    if chars[i + 1] == ':' {
                        new_text.push_str("..");
                        i += 2;
                        continue;
                    }
                }
            }
            match ch {
                '!' => new_text.push_str(",,"),
                '[' => new_text.push('«'),
                ']' => new_text.push('»'),
                '<' => new_text.push('['),
                '>' => new_text.push(']'),
                '-' => new_text.push('~'),
                '_' => new_text.push('-'),
                _ => new_text.push(ch),
            }
            i += 1;
        }

        *text = new_text;
    }

    fn transform_identifier(text: &mut String, kind: Option<CompletionItemKind>) {
        *text = text.replace("_", "-");
        let chars: Vec<_> = text.chars().collect();
        if !chars[0].is_uppercase() {
            return;
        }

        // SCREAMING_SNAKE_CASE to screaming-snake-case--c
        if let Some(kind) = kind {
            let b = match kind {
                CompletionItemKind::CONSTANT => "--c",
                CompletionItemKind::VALUE => "--g",
                _ => "",
            };
            if !b.is_empty() {
                let raw = chars.iter().find(|a| a.is_lowercase());
                if raw.is_some() {
                    let mut a = String::new();
                    a.push_str(text);
                    a.push_str("--r");
                    *text = a;
                    return;
                }

                let mut a = text.to_lowercase();
                a.push_str(b);
                *text = a;
                return;
            }
        }

        // UpperCamelCase to Upper-camel-case
        let mut a = String::new();
        if chars.contains(&'-') {
            a.push_str(text);
            a.push_str("--r");
            *text = a;
            return;
        }
        for (i, ch) in chars.iter().enumerate() {
            if i == 0 {
                a.push(chars[i]);
                continue;
            }
            if ch.is_uppercase() {
                a.push('-');
                a.push_str(&chars[i].to_lowercase().to_string());
            } else {
                a.push(chars[i]);
            }
        }
        *text = a;
    }

    fn transform_method(text: &mut String) {
        let pat = "fn(&self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!("    (&)  func({}", &text[pat.len() + start..]);
            return;
        }

        let pat = "fn(&mut self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!(" (&mut)  func({}", &text[pat.len() + start..]);
            return;
        }

        let pat = "fn(self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!(" (Self)  func({}", &text[pat.len() + start..]);
            return;
        }
    }
}

pub(crate) fn u_transform_signature_help(sig_help: &mut SignatureHelp) {
    for sig in sig_help.signatures.iter_mut() {
        sig.label = sig.label.replace("fn", "  ");
        sig.label = sig.label.replace("_", "-");
        sig.label = sig.label.replace(":", " ");
        sig.label = sig.label.replace("->", " ");
        sig.label = sig.label.replace("[", "«");
        sig.label = sig.label.replace("]", "»");
        sig.label = sig.label.replace("<", "[");
        sig.label = sig.label.replace(">", "]");
        sig.label = sig.label.replace("::", "..");
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct UMeta {
    pub(crate) span_pairs: Arc<FxHashMap<FileId, Vec<SpanPair>>>,
}
impl UMeta {
    pub(crate) fn add(&mut self, file_id: FileId, pairs: Vec<SpanPair>) {
        let span_pairs = Arc::make_mut(&mut self.span_pairs);
        span_pairs.insert(file_id, pairs);
    }
}
