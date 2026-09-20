# JPEG metadata — review item 10

JPEG exports now receive the same descriptive XMP packet as PNG and TIFF and embed it in an APP1 segment. This preserves creator, copyright, caption, title, keywords, ratings and the other fields supported by the existing metadata writer, including Unicode. Previously the JPEG branch never passed the packet to its encoder.

The segment uses the standard NUL-terminated XMP identifier and UTF-8 XML, as documented in [Adobe's JPEG handler](https://github.com/adobe/XMP-Toolkit-SDK/blob/main/XMPFiles/source/FileHandlers/JPEG_Handler.cpp). No new dependencies were required.

## Verification

Production exports are read back using the independent JPEG decoder in the image crate. Tests compare the extracted XMP packet with the original and parse it back into the complete metadata structure. Coverage includes Unicode names, punctuation, captions and copyright; metadata disabled; empty metadata; grayscale and sRGB output; ICC preservation; and identical decoded pixels with metadata on or off. Develop settings are excluded from the packet.

Application suite: **527 passed, 0 failed, 1 ignored**. Workspace Clippy with warnings denied passed. Formatting passed for the changed export file; the workspace-wide formatting check reported unrelated edits in other files, which were left untouched. Whitespace checks passed.

## Limits

This writer supports a standard XMP packet up to 65,502 UTF-8 bytes. Larger packets produce a clear error suggesting shorter metadata or PNG/TIFF, rather than silently dropping or truncating it. A regression verifies that this failure preserves an existing destination file through the atomic export path. Extended XMP and additional legacy EXIF/IPTC mirrors are not implemented. Third-party viewer compatibility was not tested manually.
