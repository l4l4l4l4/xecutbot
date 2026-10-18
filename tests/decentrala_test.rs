use chrono::NaiveDate;
use xecut_bot::decentrala::events_for_date;

#[test]
fn selects_only_xecut_on_target_monday_and_preserves_source_title() {
    let csv = "datum,vreme,lokacija,tema,tip,link,temaEN\n\
19-10-2026, 19:00,Xecut - Jovana Ćirilova 15,\"Zaobilazenje, cenzure\",workshop,,Anti-censorship workshop\n\
19-10-2026,18:00,KC Magacin,Other event,lecture,,Other event\n\
26-10-2026,19:00,Xecut - Jovana Ćirilova 15,Distribuirane mreze,lecture,,Distrubuted networks\n";
    let events = events_for_date(csv, NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "Zaobilazenje, cenzure");
}
