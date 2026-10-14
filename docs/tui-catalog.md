# English TUI Element Catalog and Component Model

A single .program file can contain smaller named programs. Each program owns its interface, layout, and behavior; the parent places program instances and connects declared inputs and outputs. Unix applets provide application computation. The catalog below defines the proposed language vocabulary. The runtime implements nine widget kinds: headings, text fields, lists, tables, text panels, badges, buttons, line charts, and bar charts. The star tracker uses a line chart; the CPU monitor uses a bar chart. Other element declarations remain design proposals.

# Component model

`accept` and `offer` declare typed public ports. `show` describes a program's local layout. `whenever` and `every` attach behavior. `place` instantiates a program and `connect` wires compatible ports. Definitions can be referenced before they appear; the interpreter validates the entire file before starting commands.

One renderer owns the physical terminal. Components receive a bounded region and communicate through explicit messages. Worker processes run applet pipelines without blocking keyboard input. Selectable collection entries need stable identities independent of screen position.

```text
open a screen titled "Documents"
divide the screen into two columns
place chooser on the left
place preview on the right
connect selected from chooser to file in preview

define a program named chooser
  offer selected as text
  show a list named documents
    include "README.md"
    include "DESIGN.md"
  whenever an item is chosen in documents
    send that item as selected

define a program named preview
  accept file as text
  show a text panel named contents
  whenever file arrives
    run cat with path file
    then show the result in contents
```

The current prototype uses text ports. Typed paths, numbers, records, and richer compositions are proposed extensions.

# Layout and composition

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Screen | Root display surface | `open a screen titled "Workspace"` |
| Panel | Named rectangular region | `show a panel named details` |
| Row | Horizontal children | `show a row named toolbar` |
| Column | Vertical children | `show a column named sidebar` |
| Grid | Children arranged in cells | `show a grid named dashboard with 2 columns` |
| Split | Divide available space | `divide body into two columns` |
| Resizable split | Adjust a division interactively | `allow the division in body to move` |
| Stack | One visible child at a time | `show a stack named pages` |
| Overlay | Content above another region | `show an overlay named help` |
| Scrolling region | Content larger than its bounds | `show a scrolling panel named history` |
| Tabs | Switch named views | `show tabs named sections` |
| Collapsible section | Expand or hide content | `show a collapsible panel named advanced` |
| Header | Persistent top region | `show a header named heading` |
| Footer | Persistent bottom region | `show a footer named shortcuts` |
| Divider | Separate content | `show a divider` |
| Spacer | Reserve or absorb space | `show a spacer` |
| Component instance | Place a named program | `place preview in details` |


# Text and document displays

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Label | Short text | `show a label named filename` |
| Heading | Prominent text | `show a heading named title` |
| Paragraph | Wrapped prose | `show a paragraph named description` |
| Text panel | Multiline output | `show a text panel named output` |
| Log | Append incoming lines | `show a scrolling log named activity` |
| Code viewer | Read highlighted source | `show a code panel named source` |
| Markdown viewer | Render document structure | `show a markdown panel named readme` |
| Diff viewer | Compare versions | `show a diff panel named changes` |
| Hex viewer | Inspect binary bytes | `show a hex panel named bytes` |
| Record viewer | Display labeled values | `show a details panel named properties` |
| Link | Selectable reference | `show a link named documentation` |
| Badge | Compact status | `show a badge named state` |


# Input controls

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Text field | Enter one line | `show a text field named query` |
| Password field | Mask entered characters | `show a password field named password` |
| Number field | Enter a number | `show a number field named amount` |
| Text area | Edit multiple lines | `show a text area named notes` |
| Code editor | Edit highlighted source | `show a code editor named editor` |
| Masked field | Follow an input pattern | `show a masked field named phone` |
| Checkbox | Independent yes or no | `show a checkbox named notifications` |
| Switch | On or off setting | `show a switch named watching` |
| Choice group | Choose one option | `show a choice group named format` |
| Dropdown | Compact option list | `show a dropdown named branch` |
| Slider | Adjust a bounded value | `show a slider named volume` |
| Date field | Enter or choose a date | `show a date field named deadline` |
| Time field | Enter or choose a time | `show a time field named departure` |
| Color picker | Choose a supported color | `show a color picker named accent` |
| Key binding field | Capture a supported key combination | `show a key binding field named shortcut` |


# Collections and navigation

| Element | Purpose | Proposed English |
| --- | --- | --- |
| List | Choose one item | `show a list named results` |
| Selection list | Choose several items | `show a selection list named targets` |
| File list | Choose typed paths | `show a list of paths named files` |
| Tree | Explore hierarchical records | `show a tree named outline` |
| Directory tree | Explore filesystem paths | `show a directory tree named folders` |
| Table | Display records in columns | `show a table named processes` |
| Editable table | Edit cells | `show an editable table named settings` |
| Menu | Choose an action | `show a menu named actions` |
| Menu bar | Group menus | `show a menu bar named navigation` |
| Command palette | Search available actions | `show a command palette named commands` |
| Breadcrumbs | Navigate ancestry | `show breadcrumbs named location` |
| Pagination | Navigate portions of a collection | `show page controls named pages` |


# Actions dialogs and feedback

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Button | Invoke an action | `show a button named refresh labelled "Refresh"` |
| Button group | Related actions | `show a button row named actions` |
| Form | Fields and submission | `show a form named preferences` |
| Dialog | Temporary focused interaction | `show a dialog named settings` |
| Confirmation | Explicit decision | `show a confirmation named remove` |
| Prompt | Small input request | `show a prompt named filename` |
| File picker | Choose files | `show a file picker named source` |
| Folder picker | Choose a directory | `show a folder picker named destination` |
| Tooltip | Explain an element | `show a tooltip for refresh` |
| Notification | Temporary message | `show a notification named notice` |
| Error message | Present a failure | `show an error named failure` |
| Validation message | Explain invalid input | `show a validation message for amount` |
| Status bar | Persistent operational information | `show a status bar named status` |
| Shortcut guide | Available keys | `show a shortcut guide named keys` |
| Empty state | Explain an empty collection | `show an empty message in results` |
| Loading state | Indicate pending content | `show loading in results` |


# Progress and visualizations

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Progress bar | Known work completion | `show a progress bar named download` |
| Gauge | Value within a range | `show a gauge named usage` |
| Spinner | Activity with unknown completion | `show a spinner named working` |
| Counter | Changing number | `show a counter named completed` |
| Large digits | Prominent numeric display | `show large digits named clock` |
| Sparkline | Compact value sequence | `show a sparkline named load` |
| Line chart | Values over an axis | `show a line chart named latency` |
| Bar chart | Compare categories | `show a bar chart named totals` |
| Scatter plot | Coordinate pairs | `show a scatter plot named samples` |
| Heat map | Matrix of intensities | `show a heat map named activity` |
| Calendar | Dates and annotations | `show a calendar named schedule` |
| Timeline | Ordered events or durations | `show a timeline named jobs` |
| Diagram | Nodes and connections | `show a diagram named dependencies` |
| Map | Locations on a character grid | `show a map named locations` |
| Canvas | Custom cells and shapes | `show a canvas named drawing` |


# Embedded programs and terminal extensions

| Element | Purpose | Proposed English |
| --- | --- | --- |
| Terminal pane | Interactive CLI inside a PTY | `show a terminal named shell` |
| Terminal session tabs | Switch interactive processes | `show terminal tabs named sessions` |
| Process monitor | Composed jobs and progress | `place monitor in dashboard` |
| Image viewer | Pixels when supported | `show an image named thumbnail` |
| Frame viewer | Graphical frames when supported | `show an animation named preview` |


# Shared properties and events

Fields share validation, bounds, placeholders, input limits, and autocomplete as properties. Layout properties include dimensions, constraints, spacing, borders, colors, alignment, wrapping, and scrolling. Text retains original newline bytes; logs and charts require explicit retention bounds.

```text
give files 30 percent of the width
give preview the remaining space
put a border around preview
give preview 1 column of padding
align title to the center
allow notes to wrap
keep the latest 500 lines in activity
disable submit
hide advanced
focus query
```

```text
whenever refresh is pressed
whenever query changes
whenever query is submitted
whenever an item is chosen in results
whenever a row is chosen in processes
whenever a cell is edited in settings
whenever a node is expanded in outline
whenever a tab is chosen in sections
whenever the screen is resized
whenever shell exits
```

These examples describe the larger catalog. The star tracker's supported grammar is documented beside its runnable program.

# Data boundaries and capabilities

Text panels consume text. Tables consume explicit JSON records with named columns; trees need explicit parent and child records. The runtime does not infer structures from formatted CLI listings. Expensive querying, analysis, conversion, and comparisons remain applet work.

Keyboard interaction is the baseline. Mouse interaction depends on terminal reporting support. Terminal panes need a PTY and virtual-terminal parser. Images and graphical animation need capability detection and a text fallback. Native OS dialogs, arbitrary fonts, and browser rendering are outside the character-cell core.

Existing implementations demonstrate the feasibility of these element families: [Ratatui widgets](https://ratatui.rs/examples/widgets/), [Textual widgets](https://textual.textualize.io/widget_gallery/), [Textual Input](https://textual.textualize.io/widgets/input/), [TextArea](https://textual.textualize.io/widgets/text_area/), [DataTable](https://textual.textualize.io/widgets/data_table/), [Tree](https://textual.textualize.io/widgets/tree/), [tui-term architecture](https://github.com/a-kenji/tui-term/blob/development/docs/ARCHITECTURE.md), [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), and [iTerm2 images](https://iterm2.com/documentation-images.html). These are feasibility references, not the syntax of this language.

# GitHub star tracker

The tracker defaults to `morluto/rea`, Reverse Engineer Anything. A repository selector offers a repository name to a tracker component in the same file. The tracker invokes read-only `gh repo view`, projects its JSON into a table, displays a description and status, and adds each star-count sample to a line chart.

The chart records observations made during the current session. It is not historical GitHub event data. Manual refresh and a one-minute interval add samples. Per-repository series retain the latest 120 points; changing repositories keeps their series separate. Authentication stays with the installed GitHub CLI. The app does not star repositories, post messages, or modify remote data.

[GitHub CLI repo view](https://cli.github.com/manual/gh_repo_view) defines the JSON fields used by the manifest.
