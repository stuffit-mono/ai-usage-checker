# AI Usage Checker

Claude · ChatGPT 구독 사용 한도와 API 사용액을 여러 계정에 걸쳐 보여주는 Windows / macOS 데스크톱 위젯입니다. Tauri 2로 만들었습니다.

- 항상 위에 떠 있는 작은 위젯과 트레이(메뉴바) 아이콘
- 계정마다 사용률, 초기화까지 남은 시간(지난 시간 막대)과 초기화 시각
- 최근 사용 속도로 계산한 한도 도달 예상 시간 (“이 속도면 47분 후 한도”)
- 경고/위험 기준(기본 80% / 95%) 도달과 5시간 세션 초기화 시 OS 알림
- 사용량 조회 API만 호출하므로 토큰은 소모되지 않습니다

## 지원하는 계정

| 서비스 | 보여주는 것 | 추가 방법 |
|---|---|---|
| **Claude Pro/Max** | 5시간 세션, 주간, 모델별 주간 한도 | Claude 로그인 창 (권장), sessionKey 붙여넣기, Claude Code 인증 파일 |
| **ChatGPT Plus/Pro** | Codex 5시간·주간 한도, 크레딧 | ChatGPT 로그인 창 |
| **Claude API** | 이번 달 API 사용액 (월 예산 대비 %) | Anthropic Admin 키 |
| **OpenAI API** | 이번 달 API 사용액 (월 예산 대비 %) | OpenAI Admin 키 |

- 로그인 창은 시크릿 모드라 계정마다 따로 로그인할 수 있고, Google 등 소셜 로그인 팝업도 지원합니다.
- ChatGPT의 일반 채팅 메시지 한도는 OpenAI가 조회 수단을 제공하지 않아 표시할 수 없습니다.
- API 선불 크레딧 잔액은 Anthropic·OpenAI 모두 API로 제공하지 않아 이번 달(UTC) 사용액을 보여줍니다.

로그인 정보와 키는 OS 보안 저장소(Windows 자격 증명 관리자 / macOS 키체인)에 `ai-usage-checker` 이름으로 저장되며, 각 서비스의 서버로만 전송됩니다. 로그인이 만료되면 카드에 **다시 로그인** 버튼이 나타납니다.

## 개발

필요한 도구: Node.js 18+, Rust(stable), Windows는 VS C++ Build Tools, macOS는 Xcode Command Line Tools.

```bash
npm install
npm run dev      # 개발 실행
npm run build    # 설치 파일 생성 (src-tauri/target/release/bundle/)
cd src-tauri && cargo test   # 응답 파싱 · ETA · 금액 계산 테스트
```

macOS 빌드는 Mac에서 `npm run build`를 실행하면 `.dmg`가 만들어집니다.

## 구조

```
ui/                 위젯 화면 (HTML/CSS/JS, 번들러 없음)
src-tauri/src/
  lib.rs            앱 설정, 명령(command), 로그인 창 처리
  claude.rs         claude.ai / Claude Code 사용량 조회와 응답 파싱
  chatgpt.rs        ChatGPT(Codex) 사용량 조회, 토큰 갱신
  billing.rs        Anthropic / OpenAI Admin API 이번 달 사용액
  poller.rs         주기 조회, 한도 도달 예측, 알림, 기록(history.json)
  tray.rs           트레이 아이콘과 메뉴
  config.rs         config.json과 OS 보안 저장소
```

## 주의

구독 사용량 엔드포인트는 비공식입니다 (claude.ai `/api/organizations/{org}/usage`, `/api/oauth/usage`, chatgpt.com `/backend-api/wham/usage`). 응답 형식이 바뀌면 `claude.rs`·`chatgpt.rs`의 `parse_usage`만 고치면 되도록 분리해 두었습니다.
