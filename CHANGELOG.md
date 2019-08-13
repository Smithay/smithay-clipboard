# Change Log

## Unreleased

- Add fallback to gtk primary selection, when zwp primary selection is not available

## 0.3.3 -- 2019-06-14

- Update nix version to 0.14.1

## 0.3.2 -- 2019-06-13

- Update smithay-client-toolkit version to 0.6.1

## 0.3.1 -- 2019-06-08

- Fix primary clipboard storing

## 0.3.0 -- 2019-06-07

- Add support for primary selection through `store_primary()` and `load_primary()`

## 0.2.1 -- 2019-04-27

- Remove dbg! macro from code

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
