# Problemas comuns

Os registros ficam em `logs\` ao lado do executável (um arquivo por dia; os com mais de sete dias são apagados ao iniciar). Com `screencast.exe --console` eles também aparecem no terminal. O painel (`/control`) mostra o estado de cada espectador.

## O espectador vê "Reconectando..."

A página do espectador reconecta sozinha quando a conexão cai ou quando o vídeo fica parado por cerca de 10 s (antes disso ela pede um quadro-chave novo). Causas mais comuns:

- **A janela compartilhada foi minimizada.** O Windows não entrega quadros de janela minimizada. Restaure a janela ou compartilhe o monitor.
- **A conexão de quem assiste está ruim.** No painel, veja *Perda* e *Taxa* do espectador. O transmissor baixa a taxa sozinho quando há perda (até `video_min_bitrate_kbps`); se a rede dele não aguenta nem isso, reduza `video_bitrate_kbps`.
- **O tunnel caiu.** A página e o WebSocket passam pelo Cloudflare Tunnel. Se o link nem abre, confira o serviço `cloudflared` (ou, em `managed`, o log `cloudflared`).

Trocar de aba no navegador do espectador **não** derruba a transmissão: a página ignora o tempo em segundo plano.

## O espectador não conecta (fica em "Conectando...")

A página abriu, mas a mídia não chegou. Significa que nenhum caminho UDP funcionou:

1. Confira o encaminhamento das portas UDP `udp_port_min`–`udp_port_max` no roteador e se você não está atrás de CGNAT ([guia](cloudflare.md#2-caminho-direto-para-o-vídeo-portas-udp)).
2. Configure um TURN (`[turn_cloudflare]` ou `[turn]`). Com TURN, quase todo espectador conecta.
3. Redes corporativas às vezes bloqueiam UDP inteiro. O TURN do Cloudflare também atende por TCP/TLS na porta 443, o que costuma passar.

## O painel mostra "Relay" no caminho

O espectador está usando o TURN. Funciona normalmente, mas cada GB conta no uso do Cloudflare ([custo](cloudflare.md#cloudflare-realtime-turn)). Para mais espectadores em `Direto`: IPv4 público, portas encaminhadas, IPv6 liberado.

## Sem áudio do sistema (compartilhando um monitor)

- **Primeira transmissão depois de ligar o PC:** o Windows pode levar ~20 s para ativar a captura do som do sistema. O programa espera até 30 s; o vídeo começa antes e o som entra depois.
- **Nada tocando:** a captura só envia som quando há som saindo no dispositivo padrão. Confira o dispositivo de saída padrão do Windows.
- Se o log mostrar `sem áudio do sistema; transmitindo só vídeo`, pare e compartilhe de novo.

## Sem áudio de uma janela

Compartilhando uma janela, só o som **daquele processo** (e dos filhos dele) vai junto. Alguns programas tocam som por outro processo (navegadores com várias abas, por exemplo): nesse caso compartilhe o monitor.

## "O Windows protegeu o computador" (SmartScreen)

O executável não é assinado. Clique em *Mais informações* → *Executar assim mesmo*.

## "libstdc++-6.dll não foi encontrado" (ou outra DLL)

O executável precisa das três DLLs do MinGW na mesma pasta (`libstdc++-6.dll`, `libgcc_s_seh-1.dll`, `libwinpthread-1.dll`). Use o pacote gerado por `scripts\empacotar.ps1`, que já as inclui.

## Porta em uso ao iniciar

Outra cópia do programa já está rodando (veja o ícone na bandeja) ou outro programa usa `control_port`/`public_port`. Feche a outra cópia ou troque as portas no `config.toml`.

## TURN do Cloudflare: "respondeu 404" ou "401"

- `404`: o `key_id` não é de uma TURN key (por exemplo, é o App ID do *Serverless SFU*). Crie em **Realtime → TURN Server**.
- `401`/`403`: o `api_token` está errado ou foi revogado. Gere outro na mesma TURN key.
