# Code layout

The user's layout requirement applies to every language, crate, test, example, and script authored
or changed in this repository, including all future implementation work.

- Separate logical stages with blank lines: setup, validation, construction, iteration, and return.
- Group closely related operations so the source shows the operation they encode. In binary
  encoders, keep the writes for one field or expression clause together.
- Keep related statements compact and unrelated statements visibly separated.
- Use descriptive intermediate values to expose data flow instead of nesting large expressions.
- Review readability after automatic formatting. Do not let a formatter erase intentional groups.
- Use a narrowly scoped formatter exception when required to retain a deliberate operation layout.

Follow [docs/STYLE.md](docs/STYLE.md) and the existing grouped layout in
`crates/sl-codesign/src/requirements.rs`. These conventions apply to all code going forward,
not only the requirements encoder.
