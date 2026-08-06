//! Reading instruments and multis written by Kontakt 5 and later.
//!
//! These files share nothing with the Kontakt 2 instruments in [`crate::nki`]
//! but their purpose. There is no XML: a container tree holds a preset, and the
//! preset is a second tree of typed binary chunks describing programs, groups
//! and zones. Both are read here, and both were reverse-engineered by others
//! long before this — the reference throughout is ConvertWithMoss (LGPL-3.0),
//! whose reading of the format this follows.
//!
//! The same structure serves an instrument and a multi. A `.nkm` is a bank of
//! up to sixty-four programs where a `.nki` holds one, which is a difference in
//! how many chunks to expect rather than in how to read them.

pub mod chunk;
pub mod container;
