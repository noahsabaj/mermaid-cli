use stock::load::load;
use stock::summary::render;

#[test]
fn totals_per_category() {
    let items = load(
        "sku,name,category,quantity,unit_price\n\
         A1,Nails,Fasteners,10,0.05\n\
         A2,Glue,Adhesives,2,3.5\n\
         A3,Tacks,Fasteners,4,0.25\n",
    )
    .unwrap();
    assert_eq!(render(&items), "Adhesives: 7.00\nFasteners: 1.50\nTotal: 8.50\n");
}

#[test]
fn columns_can_come_in_any_order() {
    let items = load("unit_price,quantity,category,name,sku\n2.00,3,Tools,File,T9\n").unwrap();
    assert_eq!(items[0].name, "File");
    assert_eq!(items[0].value_cents(), 600);
}

#[test]
fn a_missing_column_is_an_error() {
    let err = load("sku,name,quantity,unit_price\nA,B,1,1\n").unwrap_err();
    assert!(err.contains("category"), "{err}");
}
