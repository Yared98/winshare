# Screencast: compartilhamento de janela + áudio por app via link
App Windows em Rust (pasta do projeto = raiz, crate `screencast`). Captura UMA janela + SÓ o áudio daquele processo, transmite por WebRTC (P2P, sem SFU) a espectadores em navegador via link HTTPS do domínio do usuário (Cloudflare Tunnel, cloudflared como processo filho). Fechou o app, o link morre. Windows 10 2004+/11.
Alvo final: 1080p60, 6-8 Mbps/espectador, até ~10 espectadores. Um encode, mesmo RTP para todos os peers.

## Stack
tokio, axum (HTTP+WS), windows-capture (WGC), crate `windows` (EnumWindows, GetWindowThreadProcessId, ActivateAudioInterfaceAsync + AUDIOCLIENT_ACTIVATION_PARAMS PROCESS_LOOPBACK, INCLUDE_TARGET_PROCESS_TREE), openh264 (começar por ele; Media Foundation HW depois), opus (48kHz, 20ms), webrtc-rs (track H.264 packetization-mode=1, Constrained Baseline/Main + Opus), clap, serde, toml, tracing.

## Estrutura
Cargo.toml, config.toml.example (domínio, nome do tunnel, porta, senha opcional, max_viewers, TURN opcional), README.md
src/main.rs (CLI, orquestra, shutdown limpo Ctrl+C), config.rs, windows_list.rs, capture/{video,audio}.rs, encode/{h264,opus}.rs, rtc/{peer,broadcaster}.rs, server/{mod,signaling,api}.rs, tunnel.rs
web/control.html (só localhost: lista janelas, Iniciar/Parar, link, nº espectadores), web/viewer.html (público: <video>, mudo/volume, tela cheia, reconexão, botão "ativar som" se autoplay bloquear)
(embutir web/*.html no binário com include_str!)

## Fluxo
1 executa screencast.exe; 2 sobe axum em 127.0.0.1:PORTA + cloudflared; imprime https://tela.DOMINIO/s/<token>; abre control.html local; 3 escolhe janela, Iniciar; 4 amigos abrem link, WS de sinalização, negocia WebRTC; 5 Ctrl+C ou Parar encerra tudo.

## Segurança
/control e /api/* só em 127.0.0.1 (checar peer addr e rejeitar requisições com header Cf-Connecting-Ip ou Host diferente de localhost/127.0.0.1); o túnel só alcança /s/<token>, /viewer e WS de espectador. Token aleatório >=128 bits por execução (rand, hex); senha opcional da sala; limite configurável de espectadores.

## Fases
1 Esqueleto: axum + control.html + viewer.html + cloudflared (tunnel.rs sobe/derruba, lê estado). Aceite: viewer abre via /s/<token>.
2 Sinalização + WebRTC com fonte de teste (vídeo/áudio sintéticos). Aceite: espectador vê e ouve teste.
3 Captura WGC da janela + encode H.264 (openh264). 1080p30, latência <500ms LAN.
4 Áudio por processo (Process Loopback) + Opus; A/V sync <100ms.
5 Multi-espectador: um encode, N peers, entrada/saída sem afetar os demais.
6 Qualidade: 1080p60, bitrate ajustável no control, keyframe sob demanda (PLI/FIR), reconexão.
7 Acabamento: TURN opcional, janela fechada/minimizada, shutdown limpo, README, build release.
Riscos: janela minimizada não gera frames no WGC (avisar no control). Fora de escopo: SFU, gravação, chat, salas múltiplas, contas, macOS/Linux.
