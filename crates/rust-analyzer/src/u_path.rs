// debug:
//     add in .vimrc
//         \       'cmdline': [ 'u-analyzer', '--no-log-buffering', '--log-file', '/tmp/u' ],
//
//     touch /tmp/u
//     echo > /tmp/u && tail -f /tmp/u
//
// mainly changed files:
//     - crates/rust-analyzer/src/caps.rs
//     - crates/rust-analyzer/src/handlers/notification.rs
//     - crates/rust-analyzer/src/handlers/request.rs

use crate::global_state::{GlobalState, GlobalStateSnapshot, url_to_file_id};
use ide::FileId;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTextEdit, Position, SignatureHelp,
    TextDocumentPositionParams, Uri,
};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::sync::Arc;
use u::compile::ast::SpanPair;
use u::compile::lex::{Identifier, Keyword, Lex, Op, Pair, Span, Token, TokenCode};
use u::compile::parse::Parse;

// map *.u file url to corresponding *.rs file url
pub(crate) fn u_to_rs_url(u_uri: &mut Uri) -> String {
    let path = u_uri.path();
    let (dir, file_name) = path.rsplit_once('/').unwrap();
    let p = format!("{}/{}.rs", dir, &file_name[1..file_name.len() - 2]);
    u_uri.set_path(&p);
    p
}

// compile .u file to .rs, and generate Vec<SpanPair>
pub(crate) fn u_compile_to_rust(text: &mut String) -> Vec<SpanPair> {
    return compile_to_rust(text).unwrap_or(Vec::new());

    fn compile_to_rust(text: &mut String) -> Result<Vec<SpanPair>, Box<dyn std::error::Error>> {
        let data = std::mem::take(text);
        let mut l = Lex::new();
        l.lex(data)?;

        // check the first case that the token after . and .. is not an Identifier or integer or await,
        // insert an empty Identifier after . or ..
        let a = l.tokens.windows(2).position(|a| {
            if !matches!(a[0].code, TokenCode::Op(Op::Dot | Op::DotDot)) {
                return false;
            }
            if a[0].span.end_line != a[1].span.line {
                return true;
            }
            return !matches!(
                a[1].code,
                TokenCode::Identifier(_)
                    | TokenCode::Literal(_)
                    | TokenCode::Keyword(Keyword::Await)
                    // in use clause
                    | TokenCode::Op(Op::Star)
                    | TokenCode::Pair(Pair::BraceOpen)
            );
        });
        if let Some(index) = a {
            let token = &l.tokens[index];
            let mut t = Token::default();
            t.span = Span {
                width: 0,
                line: token.span.line,
                column: token.span.end_column + 1,
                end_line: token.span.line,
                end_column: token.span.end_column + 1,
            };
            let mut ident = Identifier::default();
            ident.span = t.span;
            t.code = TokenCode::Identifier(ident);
            l.tokens.insert(index + 1, t);
        }

        let mut p = Parse::new(l);
        p.set_magic();
        if let Err(err) = p.parse() {
            tracing::error!("{:?}", err);
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "parse error")));
        }

        let f = p.to_rs()?;
        *text = f.buf;
        // tracing::error!("{}", text);
        Ok(f.span_pairs)
    }
}

pub(crate) fn u_save_span_pairs(this: &mut GlobalState, url: &Uri, pairs: Vec<SpanPair>) {
    match url_to_file_id(&this.vfs.read().0, &url) {
        Ok(file_id) => match file_id {
            Some(file_id) => {
                this.u.add(file_id, pairs);
            }
            None => {
                tracing::error!("File in u_to_rs_position in VFS: none");
            }
        },
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
        Ok(file_id) => match file_id {
            Some(file_id) => file_id,
            None => {
                tracing::error!("File in u_to_rs_position in VFS: none");
                return;
            }
        },
        Err(err) => {
            tracing::error!("File in u_to_rs_position not found in VFS: {}", err);
            return;
        }
    };
    let pairs = match snap.u.span_pairs.get(&file_id) {
        Some(a) => a,
        None => {
            tracing::error!("{:?} not found in snap: u_to_rs_position", params.text_document.uri);
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
    params.position.character = (u_pair.rust.column + delta - 1) as u32;
    // tracing::error!("{:?}", u_pair);
    // tracing::error!("{:?}", params.position);
}

pub(crate) fn u_transform_completion_items(
    snap: &GlobalStateSnapshot,
    url: &Uri,
    items: &mut Vec<CompletionItem>,
) {
    if items.is_empty() {
        return;
    }

    let file_id = match snap.url_to_file_id(url) {
        Ok(file_id) => match file_id {
            Some(file_id) => file_id,
            None => {
                tracing::error!("File in u_to_rs_position in VFS: none");
                return;
            }
        },
        Err(err) => {
            tracing::error!("File in u_to_rs_position not found in VFS: {}", err);
            return;
        }
    };
    let pairs = match snap.u.span_pairs.get(&file_id) {
        Some(a) => a,
        None => {
            tracing::error!("{:?} not found in snap: u_transform_completion_items", url);
            return;
        }
    };

    let mut m: BTreeMap<Position, Position> = BTreeMap::new();
    for item in items {
        match &mut item.text_edit {
            Some(CompletionItemTextEdit::TextEdit(edit)) => {
                transform_position(&mut m, &mut edit.range.start, &pairs, true);
                transform_position(&mut m, &mut edit.range.end, &pairs, false);

                transform_text(&mut edit.new_text);
            }
            Some(CompletionItemTextEdit::InsertReplaceEdit(_)) => {
                // NOTE it seems this is not used
                // tracing::error!("insert {:#?}", insert);
            }
            None => {}
        }
        transform_text(&mut item.label);
        if let Some(ref mut text) = item.detail {
            if let Some(CompletionItemKind::Method) = item.kind {
                transform_method(text);
            }
            transform_text(text);
        }
        if let Some(ref mut text) = item.filter_text {
            transform_text(text);
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

    fn transform_text(text: &mut String) {
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
                let mut ident: String = chars[start_index..end_index].iter().collect();
                ident = u::rs::rl::Identifier::to_u_string_no_prefix(&ident);
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
                        // otherwise it will completes fmt.. instead of fmt
                        // new_text.push_str("..");
                        i += 2;
                        continue;
                    }
                }
            }
            match ch {
                '!' => new_text.push_str("_"),
                '[' => new_text.push_str("[`"),
                ']' => new_text.push_str("`]"),
                '<' => new_text.push('['),
                '>' => new_text.push(']'),
                '-' => new_text.push('~'),
                '_' => new_text.push('-'),
                _ => new_text.push(ch),
            }
            i += 1;
        }

        new_text = new_text.replace("self.", "s.");
        *text = new_text;
    }

    fn transform_method(text: &mut String) {
        let pat = "fn(&self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!(" (&) func({}", &text[pat.len() + start..]);
            return;
        }

        let pat = "fn(&mut self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!(" (&mut) func({}", &text[pat.len() + start..]);
            return;
        }

        let pat = "fn(self";
        if text.starts_with(pat) {
            let start = if text.as_bytes()[pat.len()] == ')' as u8 { 0 } else { 2 };
            *text = format!(" () func({}", &text[pat.len() + start..]);
            return;
        }
    }
}

pub(crate) fn u_transform_signature_help(sig_help: &mut SignatureHelp) {
    for sig in sig_help.signatures.iter_mut() {
        sig.label = sig.label.replace("_", "-");
        sig.label = sig.label.replace(": ", " ");
        sig.label = sig.label.replace(" -> ", " ");
        sig.label = sig.label.replace("[", "[`");
        sig.label = sig.label.replace("]", "`]");
        sig.label = sig.label.replace("<", "[");
        sig.label = sig.label.replace(">", "]");
        sig.label = sig.label.replace("::", "..");
        if sig.label.contains("(&self") {
            sig.label = sig.label.replace("fn ", "(&) ");
            if sig.label.contains("(&self, ") {
                sig.label = sig.label.replace("(&self, ", "(");
            } else {
                sig.label = sig.label.replace("(&self", "(");
            }
        } else if sig.label.contains("(&mut self") {
            sig.label = sig.label.replace("fn ", "(&mut) ");
            if sig.label.contains("(&mut self, ") {
                sig.label = sig.label.replace("(&mut self, ", "(");
            } else {
                sig.label = sig.label.replace("(&mut self", "(");
            }
        }
        sig.label = sig.label.replace(" fn ", " ");
        sig.label = sig.label.replace("fn ", " ");
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
