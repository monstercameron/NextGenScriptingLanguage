# CSV applets

`csv.parse` and `csv.stringify` are finite, in-process conversions. They keep
every cell as text and do not infer numbers, booleans, dates or null values.
CSV input, aggregate table-cell text, and emitted CSV text are each limited to
16 MiB. Tables are bounded to 100,000 data rows, 16,384 columns and 1,000,000
cells. Each applet's encoded JSON result has a separate 16 MiB limit that
includes the output newline. An input within the CSV byte limit can therefore
still fail if its parsed table takes more than 16 MiB to encode as JSON. The
transforms cooperatively check runtime cancellation and deadlines while
scanning fields.

`csv.parse` reads its optional `text` parameter, or raw UTF-8 stdin when `text`
is omitted. `has_headers` defaults to `true`. Its typed result has exactly this
shape:

```json
{
  "headers": ["name", "count"],
  "rows": [["Ada", "001"]]
}
```

With `has_headers: true`, the first record becomes `headers`; later records
must have the same width, and header names must be unique. With
`has_headers: false`, `headers` is empty and all records are rows with a
consistent width. A single leading UTF-8 byte-order mark is stripped before
parsing. Empty input has no records; a blank record or an explicitly quoted
empty field has one empty cell. One trailing blank record is ignored. A final
record terminator does not add another row. TAB is ordinary cell content; other
unsupported ASCII control characters fail.

The parser follows RFC 4180 quoting: commas and line breaks inside a quoted
field are retained, and an embedded quote is written as `""`. It accepts CRLF
record separators and LF separators used by Unix tools. Stringifying uses CRLF
for record separators while preserving LF and CRLF inside field values. UTF-8
is preserved byte-for-byte, including canonically equivalent NFC and NFD text;
no Unicode normalization is applied. A quote must begin a field, and after a
closing quote only a comma, record separator or end of input is valid. Bare CR
outside a quoted field, invalid UTF-8, unsupported ASCII control characters,
malformed quotes, duplicate headers and ragged records fail with status 2. See
[RFC 4180](https://www.rfc-editor.org/rfc/rfc4180) for the common CSV format.

`csv.stringify` takes one `table` JSON parameter with exactly the same `headers`
and `rows` fields. Every cell must be a string, each row must be rectangular,
and nonempty headers must be unique. It writes a header record only when
`headers` is nonempty. Fields containing commas, quotes, CR or LF are quoted;
embedded quotes are doubled. A single empty cell is written as `""`. Records end
in CRLF. An empty header list
represents a headerless table, so it can stringify the result of parsing with
`has_headers: false`.

The output of `csv.parse` is a typed record whose header and row cells are
typed text lists. `csv.stringify` accepts that same typed record and returns
typed text. The contracts are available as `core.csv_parse.json` and
`core.csv_stringify.json` under `manifests/`.

This sample reads CSV text through a typed program input, parses it, stringifies
the table canonically and emits the CSV as a JSON-encoded text result:

```text
accept contents as text
run csv.parse with text contents as table
run csv.stringify with table table as rendered
then encode rendered as json
```

The same source is in [the roundtrip example](../examples/csv/roundtrip.program).

The applets operate on one finite value per call. They do not provide CSV record
streaming, database-style joins or spreadsheet formula handling.
