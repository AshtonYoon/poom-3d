# Poom3D

**Poor man's Boom 3D.** Mac에서 나오는 소리를 실제로 측정한 공연장의 무대에서
들리는 것처럼 바꿔 주는 macOS 메뉴바 앱입니다. AirPods를 끼고 고개를 돌려도
무대는 정면에 그대로 있습니다.

상용 앱 Boom 3D와는 관계가 없습니다. 이름만 [pgrok](https://github.com/pgrok/pgrok)
(poor man's ngrok)에서 따왔습니다.

## 하는 일

- Apple Music, Spotify, 브라우저 등 Mac에서 재생되는 모든 소리를 가로챕니다.
  원래 소리는 끄고, 처리한 소리를 지금 쓰는 출력 장치로 내보냅니다.
- 입력의 왼쪽과 오른쪽 채널을 공연장 무대의 왼쪽과 오른쪽에 놓습니다. 직접음은
  더미헤드로 측정한 방향별 HRTF로, 그 뒤의 반사음과 잔향은 공연장에서 녹음한
  임펄스 응답으로 만듭니다.
- AirPods의 모션 센서로 머리 방향을 읽어 직접음을 반대로 돌립니다. 그래서 무대가
  머리를 따라오지 않고 제자리에 있습니다.

## 필요한 것

- macOS 14.2 이상 (Core Audio process tap이 이때 생겼습니다)
- [Rust](https://rustup.rs) 툴체인
- 헤드트래킹에는 모션 센서가 있는 AirPods. 없어도 나머지는 동작합니다.

macOS 26과 AirPods Pro에서만 써 봤습니다.

## 실행

```sh
scripts/run.sh
```

앱 번들(`target/Poom3D.app`)을 빌드하고 실행합니다. 터미널에는 로그가 나오고,
Ctrl-C나 메뉴의 "종료"로 끝냅니다.

처음 실행하면 macOS가 **시스템 오디오 녹음** 권한을 묻습니다. 허용해야 소리가
납니다. 거부하면 무음이 됩니다.

듣기 전에 제어 센터에서 AirPods의 **공간 음향을 꺼** 주세요. 켜 두면 HRTF가 두 번
걸리고, 머리 추적도 겹칩니다.

### 재빌드할 때마다 권한을 묻지 않게 하기

서명이 ad-hoc이면 macOS는 재빌드한 앱을 다른 앱으로 보고 권한을 다시 묻습니다.
아래를 한 번 실행하면 로그인 키체인에 자체 서명 인증서를 만들고, 그 뒤로는
`scripts/bundle.sh`가 그 인증서로 서명합니다.

```sh
scripts/make-signing-identity.sh
```

## 메뉴

| 항목 | 하는 일 |
| --- | --- |
| 켜기 | 끄면 tap 자체를 없애서 원래 소리가 그대로 납니다 |
| 울림 | 직접음 뒤에 붙는 홀 잔향의 양. 0이면 홀 없이 무대만, 끝까지 올리면 측정된 그대로 |
| 무대 폭 | 좁게(좌우 약 20°), 넓게(좌우 약 40~50°) |
| 좌석 | 앞줄, 중간, 뒷줄 |
| 헤드트래킹 | 머리를 돌려도 무대를 제자리에 둘지 |
| 지금 보는 쪽을 정면으로 | 무대를 지금 보는 방향으로 옮깁니다 |

명령줄 옵션은 `scripts/run.sh --wet 0.6`처럼 넘깁니다.

| 옵션 | 하는 일 |
| --- | --- |
| `--wet LEVEL` | 시작할 때의 울림 양, 0~1 (기본 0.4) |
| `--off` | 꺼진 상태로 시작 |
| `--probe` | tap과 aggregate device만 만들어 보고 정보를 출력한 뒤 종료 (소리는 건드리지 않음) |
| `--seconds N` | N초 뒤 종료 |

## 알아둘 점

- **음악용입니다.** 처리 자체가 더하는 지연은 거의 없지만 블루투스 지연은 그대로라,
  영상은 입 모양과 소리가 어긋납니다. 그럴 땐 "켜기"를 끄세요.
- **공연장은 하나입니다.** 허더스필드 대학의 St. Paul's Hall이고, 측정된 청취 위치도
  한 곳입니다. "중간" 좌석은 같은 자리에서 더 먼 줄의 스피커를 들은 실측이고,
  "뒷줄"은 거기서 직접음과 초기 반사를 낮춘 가공입니다.
- 한쪽을 계속 보고 있으면 무대가 30초쯤에 걸쳐 그쪽으로 따라옵니다. 센서 드리프트를
  흡수하려는 의도된 동작입니다.
- 빠르게 고개를 돌리면 무대가 살짝 늦게 따라옵니다. 처리가 Mac에서 이뤄져서
  블루투스 지연만큼은 줄일 수 없습니다.
- 권한을 미리 확인하는 공개 API가 없어서 비공개 TCC 프레임워크를 조회합니다.
  개인용 앱이라 공증이나 App Store 배포는 고려하지 않았습니다.

## 구조

```
src/
  main.rs        제어 스레드: 파이프라인 시작, 장치 변경 시 재구성, 메뉴 이벤트 처리
  dsp.rs         오디오 스레드에서 도는 엔진: HRTF 직접음 + 홀 컨볼루션, 전환 크로스페이드
  ir.rs          임펄스 응답 준비: 리샘플링, 정렬, 무대 방향 추정, 좌석별 가공
  head.rs        머리 방향 계산
  layout.rs      HAL 버퍼와 스테레오 블록 사이의 채널 변환
  state.rs       스레드 사이에 오가는 값
  sys/           unsafe 코드는 여기에만 있습니다
    hal.rs       Core Audio: process tap, aggregate device, IOProc
    motion.rs    Core Motion: AirPods 머리 방향
    ui.rs        AppKit: 메뉴바
    tcc.rs       권한 확인
```

오디오 콜백 안에서는 메모리를 할당하거나 해제하지 않습니다. 디버그 빌드와 테스트는
`assert_no_alloc`으로 이를 강제합니다. 무대나 좌석을 바꾸면 제어 스레드가 새 컨볼버를
만들어 링버퍼로 넘기고, 오디오 스레드가 0.3초에 걸쳐 넘어갑니다.

```sh
cargo test
cargo test --release -- --ignored --nocapture   # DSP 부하 측정
```

## 쓰인 데이터

측정 데이터는 다른 분들의 것이고, 각자의 라이선스로 재배포합니다.

- **공연장 임펄스 응답**: [MAIR Library](https://github.com/APL-Huddersfield/MAIR-Library-and-Renderer),
  Applied Psychoacoustics Lab, University of Huddersfield. CC-BY-4.0.
  `assets/ir/mair/`에 KU100 더미헤드 녹음 26개와 원문 라이선스, 설명서가 있습니다.
  > Lee, H. and Millns, C. (2017), "Microphone Array Impulse Response (MAIR) Library
  > for Spatial Audio Research," 143rd Audio Engineering Society Convention, e-Brief 356.
- **HRTF**: [SADIE II Database](https://www.york.ac.uk/sadie-project/database.html),
  University of York. Apache-2.0. `assets/hrir/sadie2/`에 KU100의 수평면 측정 360개가
  수정 없이 들어 있습니다.
  > Armstrong, C., Thresh, L., Murphy, D. and Kearney, G. (2018), "A Perceptual
  > Evaluation of Individual and Non-Individual HRTFs: A Case Study of the SADIE II
  > Database," Applied Sciences 8(11). doi:10.3390/app8112029

## 라이선스

코드는 [MIT](LICENSE)입니다. `assets/` 아래의 데이터는 위에 적은 각자의 라이선스를
따릅니다.
