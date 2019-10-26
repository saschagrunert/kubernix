pub struct Node;

impl Node {
    /// Retrieve the node name
    pub fn name(number: u8) -> String {
        const PREFIX: &str = "node";
        format!("{}-{}", PREFIX, number)
    }
}
