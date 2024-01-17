# u

## changes
```
in src/main_loop.rs:
	.on::<lsp_types::notification::DidOpenTextDocument>(|this, params| {
and
	.on::<lsp_types::notification::DidChangeTextDocument>(|this, params| {
and
	.on::<lsp_types::notification::DidCloseTextDocument>(|this, params| {

	let mut params = params;
	params.text_document.uri = lsp_types::Url::parse("file:///.../.u/.../some.rs")?;

	parse some.u file, and get some.rs
	params.content_changes[0].text = some-rs-content.to_string()

compile-to-rust func() {}
to-rs-url func() {}

```
