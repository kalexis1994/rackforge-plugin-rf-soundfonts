# Third-party work this project builds on

## ConvertWithMoss

The readers for Native Instruments formats under
`crates/rf-soundfonts-engine/src/kontakt5/`, together with
`src/fastlz.rs` and the NCW decoder, are ported from
[ConvertWithMoss](https://github.com/git-moss/ConvertWithMoss) by Jürgen
Moßgraber, which is licensed under the **GNU Lesser General Public License,
version 3**.

None of these formats is documented publicly. Native Instruments has published
no specification for the Kontakt container, its preset chunks, or its
compressed wave format, and every reader that exists was written by people who
worked the layouts out from the files themselves. ConvertWithMoss is the most
complete and best maintained of them, and this port would not have been
possible without it.

The LGPL-3.0 permits its work to be used under the terms of the GNU General
Public License, version 3, which is why this project is **GPL-3.0-or-later**
rather than the GPL-2.0-or-later it began as: LGPL-3.0 and GPL-2.0 are not
compatible, so the older option had to go once this code arrived.

Where the port departs from its reference it says so in a comment, and the
departures are about refusing malformed input rather than about the formats.
