/// One stocked product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub sku: String,
    pub name: String,
    pub category: String,
    pub quantity: u32,
    /// Price per unit, in cents.
    pub unit_cents: u64,
}

impl Item {
    /// What the units on the shelf are worth, in cents.
    pub fn value_cents(&self) -> u64 {
        self.unit_cents * u64::from(self.quantity)
    }
}
