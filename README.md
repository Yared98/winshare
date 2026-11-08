# winshare

Compartilhe uma janela ou um monitor do Windows, com áudio, para quem estiver no navegador, por um link. Não precisa instalar nada do lado de quem assiste.

O programa roda no PC que transmite (Rust, Windows 10 2004+ ou 11), captura a imagem com o Windows Graphics Capture, codifica em H.264 e Opus e envia por WebRTC direto para cada espectador. O link público pode passar por um Cloudflare Tunnel no seu domínio.

## O que faz

- **Janela ou monitor inteiro.** Numa janela, transmite só o áudio daquele programa (Process Loopback). Num monitor, transmite o áudio de todo o sistema.
- **Um encoder para todos.** O vídeo é codificado uma vez e distribuído para cada espectador por uma fila própria. Um espectador lento perde quadros sem atrasar os outros.
- **Rede instável.** O navegador pode pedir reenvio de pacotes perdidos (NACK) e um quadro completo novo (PLI). A taxa de vídeo se ajusta pela perda do pior espectador, entre um mínimo e um teto configuráveis.
- **NAT e CGNAT.** Aceita TURN fixo ou o TURN efêmero do Cloudflare Realtime.
- **Painel local** em `http://127.0.0.1:7878/control`:
  - preview do que está sendo transmitido;
  - link com botão de copiar;
  - escolha de janela ou monitor;
  - saúde de cada espectador: caminho direto ou relay, RTT, perda, fps, kbps e reconexões.
- **Ícone na bandeja** com abrir painel, copiar link, parar, iniciar com o Windows e sair. Tem reinício automático em caso de falha e instância única.
- **Logs diários** em `logs/`, ao lado do executável, com o motivo de cada desconexão. Os IPs aparecem mascarados.

## Segurança

- **Duas portas.** A porta pública (`public_port`) só serve a página do espectador e o WebSocket dele. O painel e a API (`/control`, `/api/*`) existem apenas na porta de controle, em `127.0.0.1`. Eles recusam requisições que venham por proxy ou com outro `Host`.
- **Link imprevisível.** O link do espectador leva um token aleatório de 128 bits, gerado a cada execução. Fechou o programa, o link morre.
- **Senha e limite.** A senha da sala é opcional, e há um limite de espectadores.

## Requisitos

- Windows 10 2004+ ou Windows 11.
- [Rust](https://rustup.rs) estável (edition 2024), para compilar.
- Opcional: um domínio na Cloudflare com Cloudflare Tunnel, para o link público por HTTPS.

## Compilar e rodar

```bash
cargo build --release
```

```bash
copy config.toml.example config.toml
```

Ajuste o `config.toml` e rode `target\release\screencast.exe`. Na primeira execução, o painel abre no navegador. Para ver o console de depuração na versão release, use `screencast.exe --console`.

No painel, escolha uma janela ou um monitor e clique em **Compartilhar**. Depois envie o link para quem vai assistir.

## Configuração

Todas as opções estão comentadas em [`config.toml.example`](config.toml.example). As principais:

| Opção | Padrão | Para que serve |
|---|---|---|
| `public_host` | `localhost` | Domínio que aparece no link. |
| `public_port` / `control_port` | `8080` / `7878` | Porta pública e porta do painel. |
| `tunnel` | `external` | `external`: o tunnel roda fora (ex.: no roteador). `managed`: o programa sobe o `cloudflared`. `none`: só rede local. |
| `password` | sem senha | Senha da sala. |
| `max_viewers` | `10` | Limite de espectadores simultâneos. |
| `video_bitrate_kbps` / `video_min_bitrate_kbps` | `3000` / `800` | Teto e piso da taxa adaptativa. |
| `keyframe_interval_secs` | `5` | Intervalo entre quadros completos periódicos. |
| `udp_port_min` / `udp_port_max` | `50000`–`50100` | Portas UDP do WebRTC (encaminhe no roteador para conexão direta). |
| `[turn]` / `[turn_cloudflare]` | desligado | Relay para espectadores atrás de NAT restritivo. |

O `config.toml` está no `.gitignore`. Não versione credenciais.

## Iniciar com o Windows

Pelo menu da bandeja, marque **Iniciar com o Windows**. Isso grava a entrada `winshare` em `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` com `--supervised`. Nesse modo, o programa reinicia sozinho se cair, com espera crescente de 2 s até 60 s. Ele fica só na bandeja, sem abrir o navegador. Para sair de vez, use **Sair** no menu.

## Limitações

- **Janela minimizada:** o Windows não entrega quadros, e o painel avisa. Compartilhe o monitor se precisar trocar de janela.
- **Primeira ativação do áudio do sistema:** pode levar até ~20 s enquanto o dispositivo de saída acorda.
- **Codificação por software:** o H.264 usa OpenH264, sem aceleração de hardware.
- **Sem SFU:** cada espectador recebe uma conexão direta do PC, então o upload cresce com o número de espectadores.

## Estrutura

```
src/
  main.rs            CLI, servidores HTTP, bandeja, supervisor
  config.rs          leitura e validação do config.toml
  capture/           WGC (janela e monitor) e áudio (Process Loopback e loopback do sistema)
  encode/            H.264 (OpenH264) e Opus
  rtc/               conexões WebRTC, distribuição por espectador, controle de taxa
  server/            rotas, sinalização WebSocket, API do painel
  tray.rs            ícone na bandeja, instância única, iniciar com o Windows
  supervisor.rs      reinício automático
web/
  control.html       painel local
  viewer.html        página do espectador
```

## Testes

```bash
cargo test
```
