# Optional private test fixtures

The public repository contains no photographs, sidecars, or personal application
state. Tests use synthetic data by default. Some existing camera integration tests
return early when their optional local `raws/` corpus is absent; a passing default
suite therefore does not establish real-camera coverage.

To run those tests locally, supply your own reference photographs under `raws/`.
Camera-specific tests use these neutral names:

- `leica-reference.dng`: Leica M10-R, upright orientation.
- `sony-reference.ARW`: Sony RX100M4, rotated 270 degrees.
- `fuji-reference.RAF` and `fuji-reference_camera-preview.jpg`: matching raw and preview.
- `nikon-reference.nef`: upright Nikon raw.
- `canon_eos_r_54.cr3`: Canon EOS R reference.
- `rendered-reference.jpg`: rendered image without EXIF.

Other corpus tests inspect any supported files in that local directory. Exact
camera values in existing assertions describe the original reference conditions;
arbitrary replacement files may legitimately fail those assertions.

The sidecar compatibility test reads external files only when explicitly supplied:

```sh
MONOPRO_TEST_SIDECARS=/path/to/private/sidecars cargo test -p raw-core optional_external_sidecars_open
```

Keep fixtures outside version control. The ignore rules are a convenience, not a
content scanner: never force-add private files, and review new images and screenshots
for metadata and visible personal information before committing them.
