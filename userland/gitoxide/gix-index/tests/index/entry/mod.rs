mod flags {
    use gix_index::entry::{Flags, Stage};

    #[test]
    fn from_stage() {
        for stage in [Stage::Unconflicted, Stage::Base, Stage::Ours, Stage::Theirs] {
            let actual = Flags::from_stage(stage);
            assert_eq!(actual.stage(), stage);
            let actual: Flags = stage.into();
            assert_eq!(actual.stage(), stage);
        }
    }
}
mod mode;
mod stat;
mod time;
