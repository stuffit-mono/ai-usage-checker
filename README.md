# AI Usage Checker

Claude Pro/Max 구독 사용량(5시간 세션 · 주간 · 모델별 한도)을 여러 계정에 걸쳐 보여주는 Windows / macOS 데스크톱 위젯입니다. Tauri 2로 만들었습니다.

- 항상 위에 떠 있는 작은 위젯과 트레이(메뉴바) 아이콘
- 계정마다 사용률, 초기화까지 남은 시간과 초기화 시각
- 최근 사용 속도로 계산한 한도 도달 예상 시간 (“이 속도면 47분 후 한도”)
- 경고/위험 기준(기본 80% / 95%) 도달과 5시간 세션 초기화 시 OS 알림
- 사용량 조회 API만 호출하므로 Claude 토큰은 소모되지 않습니다

## 계정 추가 방법

| 방법 | 설명 |
|---|---|
| **Claude 로그인으로 추가** (권장) | 시크릿 모드 로그인 창이 열립니다. 로그인하면 창이 닫히고 계정이 추가됩니다. 창마다 쿠키가 분리되어 있어 계정을 하나씩 계속 추가할 수 있습니다. 내장 창에서는 Google 로그인이 막힐 수 있으니 이메일 인증 코드 로그인을 쓰세요. |
| sessionKey 직접 입력 | 브라우저에서 claude.ai 접속 → F12 → Application → Cookies → `sessionKey` 값을 붙여넣습니다. |
| Claude Code 인증 파일 | `~/.claude/.credentials.json`(Claude Code CLI 로그인)을 읽습니다. 파일은 읽기만 합니다. |

세션 값은 OS 보안 저장소(Windows 자격 증명 관리자 / macOS 키체인)에 `ai-usage-checker` 이름으로 저장되며, 외부로는 claude.ai와 api.anthropic.com에만 전송됩니다. 로그인이 만료되면 카드에 **다시 로그인** 버튼이 나타납니다.

## 개발

필요한 도구: Node.js 18+, Rust(stable), Windows는 VS C++ Build Tools, macOS는 Xcode Command Line Tools.

```bash
npm install
npm run dev      # 개발 실행
npm run build    # 설치 파일 생성 (src-tauri/target/release/bundle/)
cd src-tauri && cargo test   # 응답 파싱 · ETA 계산 테스트
```

macOS 빌드는 Mac에서 `npm run build`를 실행하면 `.dmg`가 만들어집니다.

## 구조

```
ui/                 위젯 화면 (HTML/CSS/JS, 번들러 없음)
src-tauri/src/
  lib.rs            앱 설정, 명령(command), 로그인 창 처리
  claude.rs         claude.ai / Claude Code 사용량 조회와 응답 파싱
  poller.rs         주기 조회, 한도 도달 예측, 알림, 기록(history.json)
  tray.rs           트레이 아이콘과 메뉴
  config.rs         config.json과 OS 보안 저장소
```

## 주의

claude.ai의 사용량 엔드포인트(`/api/organizations/{org}/usage`, `/api/oauth/usage`)는 비공식입니다. 응답 형식이 바뀌면 `claude.rs`의 `parse_usage`만 고치면 되도록 분리해 두었습니다. 구형(`five_hour`, `seven_day_*`)과 신형(`limits` 배열) 형식을 모두 읽습니다.

## 다음 단계

- ChatGPT Codex 5시간/주간 한도
- Claude / OpenAI API 크레딧 (Admin API 키, 멀티 키)
