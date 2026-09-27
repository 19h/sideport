# Rust source layout

Separate declarations, validation, construction, iteration, and return values with blank lines.
Keep one logical operation per visual group. Related wire-format writes may share a line when the
line expresses one field or one prefix-expression clause. Preserve those groups with a narrowly
scoped `#[rustfmt::skip]`; the surrounding code remains formatted normally.

Use descriptive names for intermediate results. Extract a helper when it names an operation or
removes repeated decoding logic. Keep error paths adjacent to the condition they handle. Avoid
nesting large encoding expressions inside constructor arguments when intermediate values expose
the data flow more clearly.

`rustfmt.toml` sets a 120-column limit. Run `cargo fmt` and verify that logical groups remain
separated; formatting success alone does not establish readability.

The readability review assumes that refactoring preserves observable behavior (A1). Probe this
assumption with the existing parser, signing, cryptographic, and native interoperability tests;
validation claims apply to those fixtures and generated inputs. Passing tests do not prove
equivalence for every possible input.

Formatter interaction has medium impact on the requested layout: rustfmt separates grouped
encoding statements unless the function has a scoped exception. Review the requirements encoder
after formatting to verify that those groups remain intact. Temporary encoding buffers have low
impact on peak memory; scope sizing placeholders so they are released before signing begins.
