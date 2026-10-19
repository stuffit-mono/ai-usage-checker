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
| **Claude API** | 남은 크레딧(입력 시), 이번 달 API 사용액 (월 예산 대비 %) | Anthropic Admin 키 |
| **OpenAI API** | 남은 크레딧(입력 시), 이번 달 API 사용액 (월 예산 대비 %) | OpenAI Admin 키 |

- 로그인 창은 시크릿 모드라 계정마다 따로 로그인할 수 있고, Google 등 소셜 로그인 팝업도 지원합니다.
- ChatGPT의 일반 채팅 메시지 한도는 OpenAI가 조회 수단을 제공하지 않아 표시할 수 없습니다.
- API 선불 크레딧 잔액은 Anthropic·OpenAI 모두 API로 제공하지 않습니다. 지금 남은 크레딧을 한 번 입력하면 그 뒤 사용액을 빼서 잔액과 소진 예상 시점을 보여주고, 입력하지 않으면 이번 달(UTC) 사용액만 보여줍니다.

로그인 정보와 키는 OS 보안 저장소(Windows 자격 증명 관리자 / macOS 키체인)에 `ai-usage-checker` 이름으로 저장되며, 각 서비스의 서버로만 전송됩니다. 로그인이 만료되면 카드에 **다시 로그인** 버튼이 나타납니다.

## 개발

필요한 도구: Node.js 18+, Rust(stable), Windows는 VS C++ Build Tools, macOS는 Xcode Command Line Tools.

```bash
npm install
npm run dev      # 개발 실행
cd src-tauri && cargo test   # 응답 파싱 · ETA · 금액 계산 테스트
```

설치 파일은 업데이트 서명 키가 있어야 만들어집니다. 키는 `~/.tauri/ai-usage-checker.key`에 있습니다 (저장소에 넣지 말고 따로 백업하세요).

```bash
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/ai-usage-checker.key)" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" npm run build
```

## 릴리스와 자동 업데이트

앱은 6시간마다 `https://github.com/stuffit-mono/ai-usage-checker/releases/latest/download/latest.json`을 확인해서, 새 버전이 있으면 위젯 위쪽에 알림을 띄웁니다. 업데이트 파일은 서명을 검증한 뒤 설치합니다.

새 버전을 내는 순서:

1. `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`의 버전을 올리고 커밋
2. `git tag v0.3.0` → `git push origin main --tags`
3. GitHub Actions(`.github/workflows/release.yml`)가 Windows·macOS 설치 파일과 `latest.json`을 빌드해 **초안 릴리스**를 만듦
4. GitHub의 Releases에서 초안을 확인하고 **Publish release**를 누르면 사용자 앱에 업데이트가 뜸

처음 한 번은 저장소 Settings → Secrets and variables → Actions에 `TAURI_SIGNING_PRIVATE_KEY`(키 파일 내용)를 등록해야 합니다. macOS 빌드는 Apple 개발자 서명이 없어서, 처음 실행할 때 "확인되지 않은 개발자" 경고가 뜨면 우클릭 → 열기로 실행하세요.

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
  tray_icon.rs      트레이 아이콘 이미지(사용률 숫자) 그리기
  update.rs         앱 자동 업데이트
  config.rs         config.json과 OS 보안 저장소
```

## 주의

구독 사용량 엔드포인트는 비공식입니다 (claude.ai `/api/organizations/{org}/usage`, `/api/oauth/usage`, chatgpt.com `/backend-api/wham/usage`). 응답 형식이 바뀌면 `claude.rs`·`chatgpt.rs`의 `parse_usage`만 고치면 되도록 분리해 두었습니다.

## 출처

- 서비스 로고(`ui/logos.js`): [Lobe Icons](https://github.com/lobehub/lobe-icons) (MIT 라이선스). Claude, ChatGPT/OpenAI 로고와 상표는 각 회사의 것입니다.

## 저작권

© 2026 a_lonely_designer (lonelydesigners@gmail.com). All rights reserved. 사용 조건은 [LICENSE](LICENSE)를, 사용한 오픈소스의 고지는 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)를 보세요.
