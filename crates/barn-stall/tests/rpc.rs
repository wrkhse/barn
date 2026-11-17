//! The JSON-RPC 2.0 envelope: the rules a line keeps, one test each.

use barn_stall::{Answer, ErrorObject, Message, Notification, Request, RpcError, StrictReason};
use serde_json::json;

#[test]
fn a_request_reads_its_id_method_and_params() {
    let message =
        Message::parse(r#"{"jsonrpc":"2.0","id":4,"method":"job.run","params":{"job":"poll"}}"#)
            .expect("the page's job.run line");

    assert_eq!(
        message,
        Message::Request(Request {
            id: 4,
            method: "job.run".to_owned(),
            params: Some(
                json!({"job": "poll"})
                    .as_object()
                    .cloned()
                    .expect("the test's parameters are an object")
            ),
        })
    );
}

#[test]
fn a_notification_carries_no_id() {
    let message = Message::parse(
        r#"{"jsonrpc":"2.0","method":"log","params":{"level":"info","message":"hi"}}"#,
    )
    .expect("the page's log line");

    assert!(matches!(message, Message::Notification(_)));
}

#[test]
fn a_result_answer_reads_its_result() {
    let message =
        Message::parse(r#"{"jsonrpc":"2.0","id":7,"result":{"status":200}}"#).expect("an answer");

    assert_eq!(
        message,
        Message::Answer(Answer::Result {
            id: 7,
            result: json!({"status": 200}),
        })
    );
}

#[test]
fn an_error_answer_reads_its_error() {
    let message =
        Message::parse(r#"{"jsonrpc":"2.0","id":8,"error":{"code":-32001,"message":"denied"}}"#)
            .expect("an error answer");

    assert_eq!(
        message,
        Message::Answer(Answer::Error {
            id: 8,
            error: ErrorObject {
                code: -32_001,
                message: "denied".to_owned(),
                data: None,
            },
        })
    );
}

#[test]
fn _meta_is_carried_and_ignored() {
    let message = Message::parse(
        r#"{"jsonrpc":"2.0","id":2,"method":"stall.health","_meta":{"trace":"abc"}}"#,
    )
    .expect("a line with _meta");

    assert_eq!(
        message,
        Message::Request(Request {
            id: 2,
            method: "stall.health".to_owned(),
            params: None,
        })
    );
}

#[test]
fn an_id_of_zero_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":0,"method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::IdShape))
    );
}

#[test]
fn a_string_id_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":"seven","method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::IdShape))
    );
}

#[test]
fn a_negative_id_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":-1,"method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::IdShape))
    );
}

#[test]
fn a_null_id_beside_a_method_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":null,"method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::IdShape))
    );
}

#[test]
fn a_method_beside_a_result_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":1,"method":"log","result":true}"#),
        Err(RpcError::Strict(StrictReason::MethodWithAnswer))
    );
}

#[test]
fn a_method_beside_an_error_is_refused() {
    assert_eq!(
        Message::parse(
            r#"{"jsonrpc":"2.0","id":1,"method":"log","error":{"code":-32600,"message":"no"}}"#
        ),
        Err(RpcError::Strict(StrictReason::MethodWithAnswer))
    );
}

#[test]
fn params_that_are_not_an_object_are_refused() {
    for params in [json!(5), json!("x"), json!(null), json!([1, 2])] {
        let line =
            json!({"jsonrpc": "2.0", "id": 1, "method": "log", "params": params}).to_string();

        assert_eq!(
            Message::parse(&line),
            Err(RpcError::Strict(StrictReason::ParamsShape)),
            "{line}"
        );
    }
}

#[test]
fn an_answer_with_params_that_are_not_an_object_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":1,"result":true,"params":5}"#),
        Err(RpcError::Strict(StrictReason::ParamsShape))
    );
}

#[test]
fn an_answer_without_an_id_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","result":true}"#),
        Err(RpcError::Strict(StrictReason::AnswerWithoutId))
    );
}

#[test]
fn an_answer_with_neither_result_nor_error_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"2.0","id":1}"#),
        Err(RpcError::Strict(StrictReason::AnswerShape))
    );
}

#[test]
fn an_answer_with_both_result_and_error_is_refused() {
    assert_eq!(
        Message::parse(
            r#"{"jsonrpc":"2.0","id":1,"result":true,"error":{"code":-32600,"message":"no"}}"#
        ),
        Err(RpcError::Strict(StrictReason::AnswerShape))
    );
}

#[test]
fn a_missing_jsonrpc_is_refused() {
    assert_eq!(
        Message::parse(r#"{"id":1,"method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::MissingVersion))
    );
}

#[test]
fn another_jsonrpc_version_is_refused() {
    assert_eq!(
        Message::parse(r#"{"jsonrpc":"1.0","id":1,"method":"stall.health"}"#),
        Err(RpcError::Strict(StrictReason::WrongVersion))
    );
}

#[test]
fn a_message_that_is_not_an_object_is_refused() {
    assert_eq!(
        Message::parse("[]"),
        Err(RpcError::Strict(StrictReason::NotAnObject))
    );
}

#[test]
fn a_line_that_is_not_json_is_refused() {
    assert_eq!(
        Message::parse("not a line"),
        Err(RpcError::Strict(StrictReason::NotAnObject))
    );
}

#[test]
fn a_request_writes_jsonrpc_id_method_then_params() {
    let request = Message::Request(Request {
        id: 4,
        method: "job.run".to_owned(),
        params: Some(
            json!({"job": "poll"})
                .as_object()
                .cloned()
                .expect("the test's parameters are an object"),
        ),
    });

    assert_eq!(
        serde_json::to_string(&request).expect("the request writes"),
        r#"{"jsonrpc":"2.0","id":4,"method":"job.run","params":{"job":"poll"}}"#
    );
}

#[test]
fn a_notification_writes_jsonrpc_then_method() {
    let notification = Message::Notification(Notification {
        method: "log".to_owned(),
        params: None,
    });

    assert_eq!(
        serde_json::to_string(&notification).expect("the notification writes"),
        r#"{"jsonrpc":"2.0","method":"log"}"#
    );
}

#[test]
fn an_answer_writes_jsonrpc_id_then_result() {
    let answer = Message::Answer(Answer::Result {
        id: 7,
        result: json!(true),
    });

    assert_eq!(
        serde_json::to_string(&answer).expect("the answer writes"),
        r#"{"jsonrpc":"2.0","id":7,"result":true}"#
    );
}

#[test]
fn an_answer_writes_jsonrpc_id_then_error() {
    let answer = Message::Answer(Answer::Error {
        id: 1,
        error: ErrorObject {
            code: -32_010,
            message: "the version is not one barn speaks".to_owned(),
            data: Some(json!({"protocol": 2})),
        },
    });

    assert_eq!(
        serde_json::to_string(&answer).expect("the answer writes"),
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32010,"#,
            r#""message":"the version is not one barn speaks","data":{"protocol":2}}}"#
        )
    );
}

#[test]
fn every_error_says_what_went_wrong() {
    let errors = [
        (RpcError::OverLimit, "the line is over the cap"),
        (
            RpcError::PartialLine,
            "the stream ended in the middle of a line",
        ),
        (
            RpcError::Poisoned,
            "the connection is closed after a refused line",
        ),
        (
            RpcError::Io("the stream closed".to_owned()),
            "the stream closed",
        ),
    ];

    for (error, text) in errors {
        assert_eq!(error.to_string(), text);
    }

    let reasons = [
        (
            StrictReason::NotAnObject,
            "the line is not a JSON-RPC object",
        ),
        (StrictReason::MissingVersion, "jsonrpc is missing"),
        (StrictReason::WrongVersion, "jsonrpc is not 2.0"),
        (StrictReason::IdShape, "the id is not a positive integer"),
        (
            StrictReason::MethodWithAnswer,
            "a method cannot sit beside a result or an error",
        ),
        (StrictReason::ParamsShape, "params is not an object"),
        (
            StrictReason::AnswerShape,
            "an answer carries a result or an error, never both and never neither",
        ),
        (StrictReason::AnswerWithoutId, "an answer carries no id"),
        (StrictReason::NotUtf8, "the line is not UTF-8"),
        (
            StrictReason::CarriageReturn,
            "a line ends with a newline, never a carriage return",
        ),
        (
            StrictReason::TooManyBlankLines,
            "too many blank lines arrived in a row",
        ),
    ];

    for (reason, sentence) in reasons {
        assert_eq!(reason.to_string(), sentence);
    }

    assert_eq!(
        RpcError::Strict(StrictReason::IdShape).to_string(),
        "the line is not acceptable: the id is not a positive integer"
    );
}
