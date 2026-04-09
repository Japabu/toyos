use gix_date::Time;

#[test]
fn full_format() {
    assert_eq!(
        gix_date::parse("20080214T203045", None).unwrap(),
        Time {
            seconds: 1203021045,
            offset: 0,
        },
        "20080214T203045 = Feb 14, 2008 20:30:45 UTC"
    );
}

#[test]
fn with_colons_in_time() {
    assert_eq!(
        gix_date::parse("20080214T20:30:45", None).unwrap(),
        Time {
            seconds: 1203021045,
            offset: 0,
        },
        "20080214T20:30:45 = Feb 14, 2008 20:30:45 UTC"
    );
}

#[test]
fn hour_minute_only() {
    assert_eq!(
        gix_date::parse("20080214T2030", None).unwrap(),
        Time {
            seconds: 1203021000,
            offset: 0,
        },
        "20080214T2030 = Feb 14, 2008 20:30:00 UTC"
    );
}

#[test]
fn hour_minute_with_colon() {
    assert_eq!(
        gix_date::parse("20080214T20:30", None).unwrap(),
        Time {
            seconds: 1203021000,
            offset: 0,
        },
        "20080214T20:30 = Feb 14, 2008 20:30:00 UTC"
    );
}

#[test]
fn hour_only() {
    assert_eq!(
        gix_date::parse("20080214T20", None).unwrap(),
        Time {
            seconds: 1203019200,
            offset: 0,
        },
        "20080214T20 = Feb 14, 2008 20:00:00 UTC"
    );
}

#[test]
fn with_timezone() {
    assert_eq!(
        gix_date::parse("20080214T203045-04:00", None).unwrap(),
        Time {
            seconds: 1203035445,
            offset: -14400,
        },
        "20080214T203045-04:00 = Feb 14, 2008 20:30:45 -04:00"
    );
}

#[test]
fn with_space_before_timezone() {
    assert_eq!(
        gix_date::parse("20080214T203045 -04:00", None).unwrap(),
        Time {
            seconds: 1203035445,
            offset: -14400,
        },
        "20080214T203045 -04:00 = Feb 14, 2008 20:30:45 -04:00"
    );
}

#[test]
fn with_subseconds_ignored() {
    assert_eq!(
        gix_date::parse("20080214T203045.019-04:00", None).unwrap(),
        Time {
            seconds: 1203035445,
            offset: -14400,
        },
        "Subsecond precision is ignored, like Git does
    20080214T203045.019-04:00 = Feb 14, 2008 20:30:45.019 -04:00"
    );
}

#[test]
fn with_subseconds_no_timezone() {
    assert_eq!(
        gix_date::parse("20080214T000000.20", None).unwrap(),
        Time {
            seconds: 1202947200,
            offset: 0,
        },
        "20080214T000000.20 = Feb 14, 2008 00:00:00.20 UTC"
    );
}

#[test]
fn with_subseconds_colon_time() {
    assert_eq!(
        gix_date::parse("20080214T00:00:00.20", None).unwrap(),
        Time {
            seconds: 1202947200,
            offset: 0,
        },
        "20080214T00:00:00.20 = Feb 14, 2008 00:00:00.20 UTC"
    );
}
