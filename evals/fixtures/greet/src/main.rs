//! `greet [NAME]` prints a greeting. NAME defaults to "world".

fn greeting(name: &str) -> String {
    format!("Hello, {name}!")
}

fn main() {
    let mut name = String::from("world");
    for arg in std::env::args().skip(1) {
        name = arg;
    }
    println!("{}", greeting(&name));
}

#[cfg(test)]
mod tests {
    use super::greeting;

    #[test]
    fn greets_by_name() {
        assert_eq!(greeting("Ada"), "Hello, Ada!");
    }
}
