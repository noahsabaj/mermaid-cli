use median::{mean, median};

#[test]
fn median_of_odd_count_is_the_middle_value() {
    assert_eq!(median(&[5, 1, 3]), Some(3.0));
}

#[test]
fn median_of_even_count_averages_the_middle_pair() {
    assert_eq!(median(&[4, 1, 3, 2]), Some(2.5));
}

#[test]
fn median_of_nothing_is_none() {
    assert_eq!(median(&[]), None);
}

#[test]
fn mean_averages_everything() {
    assert_eq!(mean(&[1, 2, 3, 6]), Some(3.0));
}
