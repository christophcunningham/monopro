# PNG Unicode metadata — review item 9

Implemented locally on 2026-09-20.

PNG Author, Copyright and Description mirrors now use uncompressed UTF-8 iTXt chunks. Previously these values were copied into Latin-1 tEXt chunks, which caused the whole export to fail on an em dash, curly quotes, non-Latin names or emoji. The existing uncompressed XMP packet is preserved.

Regression tests export and decode actual files through the production writer. They verify exact Unicode text preservation for all three fields, one chunk per field, XMP text and XML escaping, and no metadata chunks when metadata export is disabled. Coverage includes 8-bit and 16-bit grayscale and sRGB output; decoded pixels are identical with metadata enabled and disabled. The existing ASCII metadata test also passes with iTXt mirrors.

Metadata readers limited to legacy tEXt chunks may no longer show the mirrors; UTF-8-capable readers can read iTXt or the existing XMP packet. Third-party viewer compatibility was not tested manually.

Validation: **525 application tests passed, 0 failed, 1 ignored**. Workspace Clippy with warnings denied, formatting and whitespace checks passed.
