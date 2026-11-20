# Encoded fixture media

These repository-created procedural PNGs contain only deterministic color patterns.
There is no user/account media or third-party artwork. Source dimensions, by filename:
`0.png` 480×320, `1.png` 240×320, `2.png` 320×320, `3.png` 570×320,
`4.png` 426×320. `FixtureMedia` selects by synthetic asset number modulo five.

Modulo seventeen, asset 1 has absent bytes, asset 2 unsupported bytes, and asset 3 an
injected download failure. Other assets supply these encoded PNGs. Every fifth synthetic
asset is a video and uses the ordinary preview-only video viewer. These cases exercise
provider/decoder/error plumbing; they do not benchmark real-world codecs or photographs.
