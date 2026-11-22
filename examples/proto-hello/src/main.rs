//! Build an order, encode it, decode it as a view that borrows from the
//! bytes, and read it back.
//!
//! ```text
//! cargo run -p proto-hello
//! ```

mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}

use giong_proto::{Encode, Enum};
use pb::shop::v1::{order, Card, LineItem, Order, OrderView, Status};

/// Encodes an order built from borrowed parts: nothing is copied until
/// `encode` writes the bytes.
fn encode_order() -> Vec<u8> {
    let items = [
        LineItem {
            sku: "TEA-001",
            quantity: 2,
            unit_price_cents: 450,
            ..Default::default()
        },
        LineItem {
            sku: "CUP-014",
            quantity: 1,
            unit_price_cents: 1200,
            ..Default::default()
        },
    ];
    let metadata = [("channel", "web"), ("locale", "vi-VN")];
    let card = Card {
        last4: "4242",
        brand: "visa",
        ..Default::default()
    };
    let order = Order {
        id: 1001,
        customer: "Nguyễn Văn A",
        items: &items,
        status: Enum::from(Status::Paid),
        metadata: &metadata,
        payment: Some(order::Payment::Card(&card)),
        note: Some("leave at the door"),
        coupon_ids: &[7, 9],
        ..Default::default()
    };
    order.encode_to_vec().expect("an order is far below 2 GiB")
}

fn main() -> Result<(), giong_proto::Error> {
    let bytes = encode_order();
    println!(
        "encoded {} bytes for {}",
        bytes.len(),
        pb::shop::v1::orders::GET_ORDER.path
    );

    // Checks the whole message once; every accessor after this is infallible
    // and returns data borrowed from `bytes`.
    let order = OrderView::decode(&bytes)?;
    println!(
        "order {} for {} is {:?}",
        order.id(),
        order.customer(),
        order.status()
    );
    let mut total = 0;
    for item in order.items() {
        println!(
            "  {} x{} at {} cents",
            item.sku(),
            item.quantity(),
            item.unit_price_cents()
        );
        total += item.unit_price_cents() * item.quantity() as i64;
    }
    println!("  total {total} cents");
    match order.payment() {
        Some(order::PaymentView::Card(card)) => {
            println!("paid by {} ending {}", card.brand(), card.last4())
        }
        Some(order::PaymentView::VoucherCode(code)) => println!("paid with voucher {code}"),
        _ => println!("not paid"),
    }
    if order.has_note() {
        println!("note: {}", order.note());
    }
    println!("channel: {:?}", order.metadata().get(&"channel"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use giong_proto::View;

    #[test]
    fn round_trip() {
        let bytes = encode_order();
        let order = OrderView::decode(&bytes).unwrap();
        assert_eq!(order.id(), 1001);
        assert_eq!(order.customer(), "Nguyễn Văn A");
        assert_eq!(order.status(), Status::Paid);
        assert_eq!(order.items().len(), 2);
        let skus: Vec<&str> = order.items().iter().map(|i| i.sku()).collect();
        assert_eq!(skus, ["TEA-001", "CUP-014"]);
        assert_eq!(order.metadata().get(&"locale"), Some("vi-VN"));
        assert!(
            matches!(order.payment(), Some(order::PaymentView::Card(c)) if c.last4() == "4242")
        );
        assert_eq!(order.note(), "leave at the door");
        assert_eq!(order.coupon_ids().to_vec(), [7, 9]);
        // A view encodes as the bytes it came from.
        assert_eq!(order.encode_to_vec().unwrap(), bytes);
        // Field by field, Google's order: the same here, since the message
        // struct also writes fields in number order.
        let mut canonical = Vec::new();
        order.encode_canonical(&mut canonical);
        assert_eq!(canonical, bytes);
    }

    #[test]
    fn strings_borrow_from_the_input() {
        let bytes = encode_order();
        let order = OrderView::decode(&bytes).unwrap();
        let range = bytes.as_ptr_range();
        assert!(range.contains(&order.customer().as_ptr()));
    }

    #[test]
    fn errors_have_codes() {
        let mut bytes = encode_order();
        bytes.truncate(bytes.len() - 3);
        let err = OrderView::decode(&bytes).unwrap_err();
        assert_eq!(err.kind(), giong_proto::ErrorKind::Truncated);
        assert!(err.to_string().starts_with("GIONG-E4001: "), "{err}");
    }
}
