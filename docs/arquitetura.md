# Arquitetura

Este documento explica como o vídeo sai da tela e chega ao navegador de quem assiste, o protocolo entre a página e o programa e por que algumas coisas são do jeito que são. Os nomes de arquivo são relativos a `src/`.

## Visão geral

```
 Windows Graphics Capture        WASAPI (processo ou sistema)
 capture/video.rs                capture/audio.rs, capture/system_audio.rs
          │ quadros BGRA                    │ PCM 48 kHz
          ▼                                 ▼
 rtc/video_loop.rs               rtc/audio_loop.rs
   encode/h264.rs (OpenH264)       encode/opus.rs
          │ amostras H.264                  │ amostras Opus
          ▼                                 ▼
 rtc/fanout.rs: FanoutTrack (uma fila e uma track por espectador)
          │
          ▼
 rtc/peer.rs: uma RTCPeerConnection por espectador ──UDP (direto ou TURN)──► navegador
          ▲
 server/signaling.rs: WebSocket que troca oferta, resposta e candidatos ICE
```

Uma transmissão (`rtc/broadcaster.rs`, tipo `Broadcaster`) tem uma captura e um encoder, e eles não dependem de quantos espectadores há. Cada espectador que entra ganha a própria `RTCPeerConnection` e as próprias tracks, alimentadas pelo `FanoutTrack`.

## Caminho do vídeo, passo a passo

1. **Captura.** `capture/video.rs` usa o Windows Graphics Capture (crate `windows-capture`) numa janela (`hwnd`) ou num monitor. Cada quadro chega como BGRA. Janela minimizada não gera quadros; o laço percebe e avisa no painel.
2. **Codificação.** `rtc/video_loop.rs` roda numa thread própria. Converte BGRA em YUV, codifica em H.264 com o OpenH264 (`encode/h264.rs`) e limita a taxa de quadros. Força um quadro-chave (IDR) a cada `keyframe_interval_secs` e sempre que algum espectador pede.
3. **Distribuição.** `FanoutTrack::write` copia cada amostra para a fila de cada espectador (64 amostras). Fila cheia: aquele espectador perde a amostra e o programa pede um quadro-chave para ele se recuperar. Os outros não são afetados.
4. **Envio.** Cada fila tem uma tarefa que escreve na `TrackLocalStaticSample` daquele espectador; a pilha WebRTC (crates `webrtc`/`rtc` 0.21) empacota em RTP, cifra (SRTP) e envia.
5. **Retorno (RTCP).** `rtc/peer.rs` instala um interceptor que lê os relatórios do navegador:
   - PLI/FIR (pedido de quadro completo) → `BroadcasterShared::request_keyframe` (no máximo um a cada 500 ms);
   - Receiver Report (perda) → `rtc/bitrate.rs`, que ajusta a taxa do encoder.

O áudio segue o mesmo caminho em `rtc/audio_loop.rs`: Process Loopback (só o som do processo da janela e dos filhos dele) ao compartilhar janela, loopback do dispositivo padrão ao compartilhar monitor, e um tom de teste na fonte sintética. Tudo vira Opus de 20 ms a 48 kHz.

## Controle de taxa

`rtc/bitrate.rs` (`BitrateController`) usa a **pior** perda entre os espectadores:

- perda acima de 10%: taxa × 0,85;
- perda abaixo de 2% por 4 s seguidos: taxa × 1,08;
- no máximo uma mudança por segundo, sempre entre `video_min_bitrate_kbps` e `video_bitrate_kbps`.

Esses valores vêm do `config.toml` pelo `Config::video_settings()` (`VideoSettings`), passado ao `Broadcaster` na hora de iniciar.

## Rotas HTTP e as duas portas

| Porta | Rotas | Arquivo |
|---|---|---|
| `public_port` (0.0.0.0) | `/s/{token}` (página), `/viewer`, `/ws/{token}` (WebSocket) | `server/mod.rs`, `server/signaling.rs` |
| `control_port` (127.0.0.1) | `/control`, `/api/windows`, `/api/monitors`, `/api/start`, `/api/stop`, `/api/status`, `/api/preview` | `server/api.rs` |

O `token` tem 128 bits aleatórios e muda a cada execução. A porta de controle recusa requisições com `Host` diferente de localhost ou com cabeçalhos de proxy, para que o tunnel nunca exponha o painel.

## Protocolo do WebSocket

Mensagens JSON com um campo `type`. O **programa** faz a oferta; o navegador responde.

| Direção | `type` | Conteúdo |
|---|---|---|
| programa → navegador | `auth_required` | Só se a sala tem senha. O navegador tem 30 s para responder. |
| navegador → programa | `auth` | `password` |
| programa → navegador | `offer` | `sdp` e `ice_servers` (STUN e, se configurado, TURN com credenciais temporárias) |
| navegador → programa | `answer` | `sdp` |
| ambos | `ice` | `candidate` (candidatos ICE incrementais) |
| navegador → programa | `keyframe` | Pede um quadro completo (a página manda quando o vídeo trava). |
| navegador → programa | `stats` | A cada 5 s: caminho (direto/relay), RTT, perda, fps, kbps, reconexões. Até 4 KB; os valores são saneados em `server/viewer_stats.rs`. |
| navegador → programa | `ping` | A cada 2 s, para o Cloudflare não fechar a conexão ociosa. |
| programa → navegador | `error` | `message` (senha errada, sala cheia, transmissão encerrada). |

Mensagens acima de 64 KB são descartadas. A página reconecta sozinha quando a conexão cai ou o vídeo fica parado (`web/viewer.html`).

O preview do painel (`/api/preview`) usa o mesmo protocolo, sem senha e sem ocupar vaga de espectador, e só aceita conexões cuja `Origin` seja local.

## Estado do servidor

`server/mod.rs` (`AppState`) guarda a configuração, o token, a transmissão ao vivo (`LiveSession`: `Broadcaster`, canal de parada e lista ICE atual), o contador de vagas, o conjunto de portas UDP (`PortPool`) e o registro de saúde dos espectadores (`server/viewers.rs`), que o painel lê por `/api/status`.

Quando um espectador entra, `AppState::join_viewer` reserva a vaga e devolve as tracks novas, o canal de parada, os servidores ICE e o `BroadcasterShared`. A sinalização passa esse `BroadcasterShared` ao `Peer::new`, que liga o RTCP daquele espectador ao controle de taxa e aos pedidos de quadro-chave.

## Processo, bandeja e supervisor

- `main.rs`: argumentos, logs diários em `logs\` (apagados após 7 dias), servidores, tunnel (`tunnel.rs`) e TURN (`turn.rs`).
- `tray.rs`: ícone na bandeja, instância única e entrada "Iniciar com o Windows".
- `supervisor.rs`: com `--supervised`, um processo pai reinicia o filho se ele cair (espera de 2 s a 60 s). O filho recebe `WINSHARE_SUPERVISED` e não abre o navegador.

## Decisões de projeto

- **Uma track por espectador.** Na versão 0.21 do `webrtc`, uma `TrackLocalStaticRTP/Sample` só fica ligada a uma conexão. Com uma track compartilhada, quem reconectava ficava sem vídeo. O `FanoutTrack` cria uma track e uma fila por espectador.
- **Um encoder só.** Codificar uma vez e distribuir custa a mesma CPU com 1 ou 10 espectadores. Em troca, a taxa segue o pior espectador.
- **Sem SFU.** Cada espectador recebe os dados direto do PC: o upload cresce com o número de espectadores, mas não há servidor intermediário para manter ou pagar.
- **Duas portas.** A pública só tem a página e o WebSocket; o painel fica em 127.0.0.1. Um erro de configuração do tunnel não expõe o controle.
- **Preview local sem senha.** O painel já é só local; o preview é um espectador como os outros, mas sem vaga e sem senha.
- **TURN efêmero.** A chave da API do Cloudflare fica no programa; o navegador recebe só credenciais de 24 h.
- **Toolchain GNU.** O OpenH264 é C++ e o projeto compila com MinGW. Por isso o executável depende de `libstdc++-6.dll`, `libgcc_s_seh-1.dll` e `libwinpthread-1.dll` (`scripts\empacotar.ps1` as copia).
