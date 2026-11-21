//! Gióng's own integration tests, run inside the simulation (see build.rs).
//! `GIONG_SIM_FAULTS=benign` runs them under benign faults;
//! `GIONG_SIM_SEED` picks the seed (default 1).

include!(concat!(env!("OUT_DIR"), "/suite/tests/index.rs"));
