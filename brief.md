# cap2voice — Project Brief

> Trạng thái: **Draft — tầm nhìn đã chốt, chờ viết spec rồi build**
> Ngày: 2026-11-19
> Người quyết: Danh
> Repo: `E:\Code\github\cap2voice`

## 1. Vấn đề

Nhiều video tiếng Trung (phim, drama, Bilibili) chỉ có phụ đề cứng
(hardcoded, cháy thẳng vào hình ảnh), không có file phụ đề rời để bật/tắt
hay dịch. Người xem đang học tiếng Trung phải vừa đọc chữ Trung vừa đoán
nghĩa, không có ai đọc dịch lên thành tiếng.

## 2. Mục tiêu

Desktop app Windows giúp xem video có phụ đề cứng tiếng Trung:

- Tự đọc phụ đề từ hình ảnh video (OCR), theo thời gian thực.
- Dịch sang tiếng Việt, hiện cả hai dòng (Trung trên, Việt dưới).
- Đọc bản dịch tiếng Việt lên thành tiếng (TTS).
- Xem được mọi nguồn video: file local, player, video trên browser.

Không phải trình phát video. App chỉ quan sát màn hình, không chạm vào
file video hay trình duyệt.

## 3. Cơ chế

Theo repo mẫu [`pijuskri/bilibili-caption-project`](https://github.com/pijuskri/bilibili-caption-project)
(dịch phụ đề Bilibili Trung → Anh theo thời gian thực):

1. Người dùng kéo chuột chọn vùng phụ đề trên màn hình.
2. Chụp vùng đó liên tục (repo mẫu: 10 khung/giây, dxcam).
3. OCR vùng ảnh, lọc chỉ giữ chữ Trung.
4. Chống trùng: chỉ dịch khi câu mới khác câu gần nhất
   (repo mẫu: so khớp mờ stroke-based, ngưỡng 0.93).
5. Dịch, hiện lên cửa sổ nổi luôn trên cùng, ghi log `câu gốc : câu dịch`.

Khác repo mẫu:

- OCR: **wcocr-rs** (engine OCR của WeChat, đã viết xong và test thật
  trên PC 19/11) thay cho PaddleOCR — khỏi cài PaddlePaddle + PyTorch +
  CUDA, không cần GPU.
- Dịch Trung → Việt thay vì Trung → Anh.
- Thêm TTS tiếng Việt, hiện cả hai dòng, thêm bước lọc pixel trước OCR
  (chỉ OCR khi vùng phụ đề đổi hình, repo mẫu OCR mọi khung hình).
- Giao diện desktop hoàn chỉnh (repo mẫu chỉ có tkinter thô).

## 4. Quyết định đã chốt

| Ngày | Quyết định |
| ---- | ---------- |
| 19/11 21:30 | Hiện cả hai dòng (Trung + Việt), TTS đọc tiếng Việt |
| 19/11 21:36 | Dùng Rust hoàn toàn, giao diện cũng Rust, không Tauri, không webview |
| 19/11 21:37 | Dịch ban đầu dùng Google Translate, TTS dùng Edge-TTS |
| 19/11 21:39 | Dịch + TTS tạm thời chạy bằng Python API server local, sau này thay bằng Rust thuần |
| 19/11 21:41 | Phải có màn Cài đặt; có chế độ ẩn xuống khay hệ thống (system tray) |

## 5. Kiến trúc tổng thể

```text
Màn hình (player / browser bất kỳ)
   │  chụp vùng phụ đề đã chọn
   ▼
App Rust (egui) ──► wcocr-rs (OCR, thư viện nội bộ)
   │  câu Trung mới (sau chống trùng)
   ├─► Python API server (localhost, tạm thời)
   │      ├─ POST /translate  (zh → vi, Google Translate)
   │      └─ POST /tts        (Edge-TTS → mp3, giọng vi-VN-NamMinhNeural)
   ▼
Overlay nổi: dòng Trung + dòng Việt ──► loa đọc dòng Việt (rodio)
   │  ghi log từng cặp câu (Trung : Việt) ra file
```

App Rust tự bật/tắt Python server khi mở/đóng app, người dùng không
chạy tay. API server là lớp tạm: sau này thay 2 endpoint bằng code Rust
mà không đổi giao diện hay luồng OCR.

### Crate / thư viện đã kiểm chứng có thật (19/11, crates.io)

- `eframe` 0.37 — giao diện egui, cửa sổ nổi không viền, luôn trên cùng.
- `xcap` 0.9.10 — chụp màn hình vùng chọn. Dự phòng: `windows-capture`
  2.0.1 (DirectX, gần với dxcam của repo mẫu nhất).
- `reqwest` 0.13 — gọi Python API server qua HTTP.
- `rodio` 0.23 — phát tiếng TTS.
- Không có crate `edge-tts` — khi viết TTS bằng Rust thuần sẽ phải tự
  gọi protocol websocket của Edge-TTS.

### Python API server (tạm thời)

- Framework: FastAPI (hoặc stdlib nếu muốn gọn).
- `POST /translate { "text": "<câu Trung>" }` → `{ "text": "<câu Việt>" }`
- `POST /tts { "text": "<câu Việt>", "voice": "vi-VN-NamMinhNeural" }`
  → audio mp3.
- `GET /health` → trạng thái server.
- Thư viện: `edge-tts`, gọi Google Translate (endpoint miễn phí,
  chưa cần key).

## 6. Giao diện

Mockup xem trước: `docs/mockups/` (3 ảnh).

1. **Overlay phụ đề** (`overlay-main`): panel nổi nửa trong suốt đè lên
   phía dưới video. Dòng Trung trắng, dòng Việt xanh lá. Hàng nút:
   Chọn vùng, Bắt đầu, Dừng, loa, chọn giọng đọc.
2. **Chọn vùng** (`region-select`): màn hình tối lại, kéo chuột khoanh
   vùng phụ đề bằng khung đỏ đứt nét, hiện kích thước vùng đã chọn.
3. **Cài đặt + khay hệ thống** (`settings-tray`): cửa sổ Cài đặt có
   sidebar Chung / Dịch / Giọng đọc / Hiển thị / Phím tắt. Các mục:
   giọng đọc, âm lượng, tốc độ chụp, vùng phụ đề + nút chọn lại vùng,
   cỡ chữ phụ đề, tự động đọc phụ đề, khởi động cùng Windows,
   "Ẩn xuống khay hệ thống khi đóng cửa sổ". Menu chuột phải ở tray:
   Hiện cửa sổ / Bắt đầu dịch / Dừng / Cài đặt / Thoát.

Hành vi tray: bấm X thì app thu về khay, vẫn dịch nền. Chỉ thoát hẳn
khi chọn Thoát trong menu tray.

## 7. Luồng chạy chi tiết

1. Mở app → tự bật Python API server, hiện overlay (hoặc thu về tray
   nếu cài đặt vậy).
2. Bấm "Chọn vùng" → màn hình tối lại, kéo chuột khoanh vùng phụ đề →
   lưu tọa độ vùng.
3. Bấm "Bắt đầu" → vòng lặp chụp vùng đã chọn (mặc định 10 fps).
4. So pixel vùng chụp với lần trước: không đổi thì bỏ qua, không OCR.
5. Vùng đổi → wcocr-rs OCR → lọc chỉ giữ chữ Trung (+ số) → bỏ qua
   nếu không có chữ Trung.
6. So với câu gần nhất (chống trùng mờ): giống thì bỏ qua, khác thì
   dịch qua `POST /translate`.
7. Hiện 2 dòng lên overlay, ghi log, gọi `POST /tts` rồi phát tiếng
   Việt. Câu đang đọc thì không đọc chồng câu mới (hàng đợi).
8. Bấm "Dừng" hoặc Thoát trong menu tray → dừng vòng lặp, tắt server.

## 8. Phạm vi

### MVP

- Chọn vùng, chụp, OCR, chống trùng, dịch Google, overlay 2 dòng,
  TTS Edge-TTS, log câu, Cài đặt, tray.

### Ngoài MVP (để sau, không làm trong đợt đầu)

- Dịch local (model `Helsinki-NLP/opus-mt-zh-vi`, ~300MB, BLEU 20) để
  chạy offline; DeepL API làm backend dịch thứ hai.
- TTS viết lại bằng Rust thuần (bỏ Python server).
- Pinyin trên dòng Trung, bấm vào câu để nghe lại tiếng Trung.
- Bảng lịch sử câu đã dịch để học lại.
- Extension browser (lấy khung hình trực tiếp thay vì chụp màn hình).

## 9. Điểm cần kiểm chứng khi build

- Tốc độ chụp của `xcap` trên PC thật; nếu chậm thì đổi
  `windows-capture`.
- Chất lượng dịch Google trên câu phụ đề thật (câu ngắn, khẩu ngữ);
  so với DeepL trước khi chốt backend dài hạn.
- Protocol websocket Edge-TTS (khi viết lại bằng Rust).
- Giới hạn tốc độ của endpoint Google Translate miễn phí.
- `wcocr-rs` dùng làm thư viện nội bộ (hiện tại xuất C ABI
  `wechat_ocr`/`stop_ocr`; cần refactor thành crate dùng trực tiếp).
- Crate tray icon trên Windows + hành vi bấm X thu về tray.

## 10. Cấu trúc repo đề xuất

```text
cap2voice/
├── brief.md            # file này
├── docs/
│   └── mockups/        # ảnh giao diện xem trước
├── crates/
│   ├── cap2voice-app/  # app egui: UI, capture, pipeline, tray
│   └── wcocr-rs/       # OCR WeChat (đã có: C:\Users\doanv\wechat-ocr-rs\)
├── server/             # Python API server tạm thời (translate + TTS)
│   ├── server.py
│   └── requirements.txt
└── .agents/            # skill / workflow của repo (thiết lập sau)
```

## 11. Workflow đề xuất

Làm theo vòng spec-first (interview → spec → plan → build → test →
review → ship), giống quy trình đã dùng cho các dự án khác:

1. **Spec**: viết `docs/spec.md` từ brief này, Danh duyệt.
   Gate: spec ghi rõ tiêu chí nghiệm thu từng tính năng.
2. **Plan**: chia ticket theo thành phần (capture, OCR, API server,
   overlay, settings, tray). Mỗi ticket có "xong là gì" + cách kiểm
   chứng trên PC thật.
3. **Build**: code trên VM trước, build + chạy thử trên PC Danh với
   video thật (phim có phụ đề cứng tiếng Trung).
4. **Test**: kiểm chứng từng điểm trong spec bằng dữ liệu thật, không
   báo xong dựa trên lời builder. Ghi kết quả test vào log.
5. **Review**: Danh duyệt bản chạy được trên PC rồi mới đóng ticket.
6. **Ship**: đóng gói exe + hướng dẫn chạy (app Rust + Python server
   đi kèm).

Quy tắc: không tự đăng/push/deploy khi chưa có lệnh rõ ràng từng lần.
Không chạm file gốc trên PC trước khi được duyệt.

## 12. Ghi chú

- Engine OCR là của WeChat (mmmojo), dùng qua Weixin đã cài trên PC
  (`C:\Program Files\Tencent\Weixin\4.0.2.28`, plugin OCR 8022).
  Chỉ dùng cá nhân, không phân phối engine.
- Google Translate endpoint miễn phí và Edge-TTS đều là dịch vụ ngoài,
  cần mạng. App báo rõ khi mất mạng thay vì lặng lẽ không dịch.
- Giọng mặc định: `vi-VN-NamMinhNeural`, đổi được trong Cài đặt.
