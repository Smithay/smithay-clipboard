# Change Log

## Unreleased

## 0.2.0 -- 2019-04-27

- `Clipboard::store()` and `Clipboard::load()` now take a `Option<String>` for the seat name, if
no seat name is provided then the name of the last seat to generate an event will be used instead

## 0.1.1 -- 2019-04-24

- Do a sync roundtrip to register avaliable seats on clipboard creation
- Collect serials from key and pointer events
- Return an empty string for load requests when no seats are avaliable

## 0.1.0 -- 2019-02-14

Initial version, including:

- `WaylandClipboard` with `new_threaded()` and `new_threaded_from_external()`
- multi seat support
