# Edge-case hunt — inline_data extractor

**Status:** DONE — hit 10-iteration cap
**Final totals:** 16 new tests, 3 bug fixes, 10 iterations.

| Iter | Attack vector | Found bug? | Test added | Code fix |
|---|---|---|---|---|
| 1 | UTF-8 BOM before JSON body | yes | application_json_body_with_utf8_bom_is_parsed | strip_prefix '\u{FEFF}' before trim() |
| 2 | `//` line-commented assignment | yes | assignment_inside_line_comment_is_not_extracted | new strip_js_comments() runs before assignment regexes |
| 3a | `/* */` block-commented assignment | no — handled by iter 2 | assignment_inside_block_comment_is_not_extracted | (no change) |
| 3b | `//` inside URL string value | no — string-aware stripping intact | url_with_double_slash_inside_string_value_is_preserved | (no change) |
| 4a | RSC push with non-numeric first arg | no — regex filters it | next_f_push_with_non_numeric_first_arg_is_ignored | (no change) |
| 4b | RSC bootstrap push `[0]` (no payload) | no — regex requires 2 args | next_f_push_with_only_one_arg_is_ignored | (no change) |
| 5 | determinism (byte-for-byte) | no — BTreeMap + doc order suffices | extract_is_deterministic_byte_for_byte | (no change) |
| 6a | object value containing only backslash `"\\\\"` | no — escape_next handling correct | object_with_string_value_containing_only_backslash_is_extracted | (no change) |
| 6b | unterminated object literal | no — returns None cleanly | unterminated_object_literal_does_not_crash_and_is_skipped | (no change) |
| 7a | assignment shape inside outer string | no — regex consumes, inner `\"` breaks parse | assignment_shape_inside_outer_string_is_not_double_extracted | (no change) |
| 7b | nested assignment in outer string value | no — same reason | nested_assignment_shape_inside_outer_unquoted_position_does_not_phantom | (no change) |
| 8 | 200-segment dotted path (ReDoS check) | no — Rust regex is linear | very_long_dotted_path_does_not_blow_up_regex | (no change) |
| 9a | duplicate `id` attributes | no — entry().or_insert() | duplicate_application_json_ids_first_wins | (no change) |
| 9b | id attribute with surrounding whitespace | no — id.trim() already used | id_attribute_with_surrounding_whitespace_is_trimmed | (no change) |
| 9c | function declaration `function f() {}` | no — no `=` so no match | function_declaration_is_not_misidentified_as_assignment | (no change) |
| 10 | `<script type="module">` ES module | yes | module_script_type_is_treated_as_javascript | accept `module` in is_plain_js_script |

**Last attempted attack:** ES module script type — found real bug (modules silently skipped)
**Notes on documented-intent edge cases (not fixed):**
- `\NNN` octal escapes inside JS strings remain unhandled by `rewrite_js_only_escapes` — the docstring lists only `\xNN`, `\v`, `\0`, `\'`. `\0` followed by `8`/`9` is similarly passed through (technically a missable case, but author chose "avoid octal misinterpretation").
- Regex literals, template literals' `${...}`, and JS comments inside object-literal bodies remain unhandled by `find_matching_brace` — explicitly documented. Real `JSON.stringify` output never contains these, so the trade-off stands.
- Single-quoted strings as JSON value carriers — explicitly not rewritten; docstring lists this as an accepted miss.

**Updated:** 2026-05-18T01:00:00Z
