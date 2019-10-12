pub struct Node;

impl Node {
    /// Retrieve the node name for the node number
    pub fn name(number: u8) -> String {
        format!("node-{}", number)
    }
}
