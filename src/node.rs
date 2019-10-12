pub struct Node;

impl Node {
    const PREFIX: &'static str = "node";

    /// Retrieve the node name for the node number
    pub fn name(number: u8) -> String {
        format!("{}-{}", Self::PREFIX, number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_success() {
        assert_eq!(Node::name(10), "node-10")
    }
}
