//! Hand-written `prost` protobuf definitions matching `ocr_common.proto`
//! and `ocr_wx4.proto` (WeChat 4.x OCR IPC wire format).

/// Messages from `ocr_common.proto` (`package ocr_common`).
pub mod ocr_common {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Point {
        #[prost(float, optional, tag = "1")]
        pub x: Option<f32>,
        #[prost(float, optional, tag = "2")]
        pub y: Option<f32>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Box {
        #[prost(message, optional, tag = "1")]
        pub topleft: Option<Point>,
        #[prost(message, optional, tag = "2")]
        pub topright: Option<Point>,
        #[prost(message, optional, tag = "3")]
        pub bottomright: Option<Point>,
        #[prost(message, optional, tag = "4")]
        pub bottomleft: Option<Point>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct OcrResultChar {
        #[prost(message, optional, tag = "1")]
        pub char_box: Option<Box>,
        #[prost(string, tag = "2")]
        pub chars: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct OcrResultLine {
        #[prost(message, optional, tag = "1")]
        pub line_box: Option<Box>,
        #[prost(string, tag = "2")]
        pub text: String,
        #[prost(float, tag = "3")]
        pub rate: f32,
        #[prost(message, repeated, tag = "4")]
        pub blocks: Vec<OcrResultChar>,
        #[prost(float, tag = "5")]
        pub left: f32,
        #[prost(float, tag = "6")]
        pub top: f32,
        #[prost(float, tag = "7")]
        pub right: f32,
        #[prost(float, tag = "8")]
        pub bottom: f32,
        #[prost(bool, optional, tag = "9")]
        pub unknown_0: Option<bool>,
        #[prost(message, optional, tag = "10")]
        pub box10: Option<Box>,
    }
}

/// Messages from `ocr_wx4.proto` (`package wx4`).
pub mod wx4 {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct OcrSupportMessage {
        #[prost(bool, optional, tag = "1")]
        pub supported: Option<bool>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ReqType {
        #[prost(bool, optional, tag = "1")]
        pub t1: Option<bool>,
        #[prost(bool, optional, tag = "2")]
        pub t2: Option<bool>,
        #[prost(bool, optional, tag = "3")]
        pub t3: Option<bool>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ParseOcrReqMessage {
        #[prost(uint64, optional, tag = "1")]
        pub task_id: Option<u64>,
        #[prost(string, optional, tag = "2")]
        pub pic_path: Option<String>,
        #[prost(uint32, optional, tag = "3")]
        pub xx3: Option<u32>,
        #[prost(uint32, optional, tag = "4")]
        pub xx4: Option<u32>,
        #[prost(bytes, optional, tag = "5")]
        pub pic_data: Option<Vec<u8>>,
        #[prost(message, optional, tag = "6")]
        pub rt: Option<ReqType>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct OcrResultInfo {
        #[prost(message, repeated, tag = "3")]
        pub lines: Vec<super::ocr_common::OcrResultLine>,
        #[prost(uint32, optional, tag = "4")]
        pub img_width: Option<u32>,
        #[prost(uint32, optional, tag = "5")]
        pub img_height: Option<u32>,
        #[prost(string, optional, tag = "6")]
        pub cpu_report: Option<String>,
        #[prost(uint64, optional, tag = "7")]
        pub time_used: Option<u64>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ParseOcrRespMessage {
        #[prost(uint64, optional, tag = "1")]
        pub task_id: Option<u64>,
        #[prost(int32, optional, tag = "2")]
        pub err_code: Option<i32>,
        #[prost(message, optional, tag = "3")]
        pub res: Option<OcrResultInfo>,
        #[prost(message, optional, tag = "4")]
        pub rt: Option<ReqType>,
        #[prost(bytes, optional, tag = "5")]
        pub qrcode: Option<Vec<u8>>,
        #[prost(bytes, optional, tag = "6")]
        pub mmfg: Option<Vec<u8>>,
    }
}

#[cfg(test)]
mod tests {
    use super::ocr_common::{Box as OcrBox, OcrResultChar, OcrResultLine, Point};
    use super::wx4::{
        OcrResultInfo, OcrSupportMessage, ParseOcrReqMessage, ParseOcrRespMessage, ReqType,
    };
    use prost::Message;

    #[test]
    fn ocr_support_message_decodes_wechat_wire_bytes() {
        let Ok(supported_true) = OcrSupportMessage::decode(&[0x08, 0x01][..]) else {
            panic!("failed to decode supported=true wire bytes");
        };
        assert_eq!(supported_true.supported, Some(true));

        let Ok(supported_false) = OcrSupportMessage::decode(&[0x08, 0x00][..]) else {
            panic!("failed to decode supported=false wire bytes");
        };
        assert_eq!(supported_false.supported, Some(false));
    }

    #[test]
    fn parse_ocr_req_roundtrip_preserves_task_id_path_and_req_type() {
        let req = ParseOcrReqMessage {
            task_id: Some(42),
            pic_path: Some(r"C:\tmp\frame.png".to_string()),
            xx3: None,
            xx4: None,
            pic_data: None,
            rt: Some(ReqType {
                t1: Some(true),
                t2: Some(true),
                t3: Some(false),
            }),
        };

        let encoded = req.encode_to_vec();
        // Field 1 (`task_id`, uint64, wire type varint) = tag byte 0x08, value 42 = 0x2a.
        assert_eq!(&encoded[..2], &[0x08, 0x2a]);

        let Ok(decoded) = ParseOcrReqMessage::decode(&encoded[..]) else {
            panic!("failed to decode ParseOcrReqMessage");
        };
        assert_eq!(decoded, req);
    }

    #[test]
    fn parse_ocr_req_roundtrip_preserves_pic_data() {
        let png_magic = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let req = ParseOcrReqMessage {
            task_id: Some(99),
            pic_path: None,
            xx3: Some(1),
            xx4: Some(2),
            pic_data: Some(png_magic.clone()),
            rt: None,
        };

        let encoded = req.encode_to_vec();
        let Ok(decoded) = ParseOcrReqMessage::decode(&encoded[..]) else {
            panic!("failed to decode ParseOcrReqMessage with pic_data");
        };
        assert_eq!(decoded.pic_data, Some(png_magic));
        assert_eq!(decoded, req);
    }

    #[test]
    fn parse_ocr_resp_roundtrip_preserves_chinese_lines_and_char_boxes() {
        let make_box = |x1: f32, y1: f32, x2: f32, y2: f32| OcrBox {
            topleft: Some(Point {
                x: Some(x1),
                y: Some(y1),
            }),
            topright: Some(Point {
                x: Some(x2),
                y: Some(y1),
            }),
            bottomright: Some(Point {
                x: Some(x2),
                y: Some(y2),
            }),
            bottomleft: Some(Point {
                x: Some(x1),
                y: Some(y2),
            }),
        };

        let resp = ParseOcrRespMessage {
            task_id: Some(7),
            err_code: Some(0),
            res: Some(OcrResultInfo {
                lines: vec![OcrResultLine {
                    line_box: Some(make_box(10.0, 20.0, 200.0, 50.0)),
                    text: "夜深了，雨还在下。".to_string(),
                    rate: 0.98,
                    blocks: vec![OcrResultChar {
                        char_box: Some(make_box(10.0, 20.0, 30.0, 50.0)),
                        chars: "夜".to_string(),
                    }],
                    left: 10.0,
                    top: 20.0,
                    right: 200.0,
                    bottom: 50.0,
                    unknown_0: Some(false),
                    box10: None,
                }],
                img_width: Some(1280),
                img_height: Some(60),
                cpu_report: Some("ok".to_string()),
                time_used: Some(18),
            }),
            rt: Some(ReqType {
                t1: Some(true),
                t2: Some(true),
                t3: Some(false),
            }),
            qrcode: None,
            mmfg: None,
        };

        let encoded = resp.encode_to_vec();
        let Ok(decoded) = ParseOcrRespMessage::decode(&encoded[..]) else {
            panic!("failed to decode ParseOcrRespMessage");
        };
        assert_eq!(decoded, resp);
    }
}
