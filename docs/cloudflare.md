# Link público com Cloudflare (tunnel, DNS, portas e TURN)

O winshare tem duas partes de rede, e elas usam caminhos diferentes:

| O quê | Por onde passa | Para que serve |
|---|---|---|
| Página do espectador e sinalização (WebSocket) | **Cloudflare Tunnel** → `public_port` (padrão 8080) | O espectador abre o link HTTPS e combina a conexão de vídeo. |
| Vídeo e áudio (WebRTC, UDP) | **Direto** entre o seu PC e o espectador (portas `udp_port_min`–`udp_port_max`) ou, se não der, pelo **TURN** | A mídia em si. |

O tunnel **não** carrega o vídeo: ele só entrega HTTP e WebSocket. Por isso, além do tunnel, a mídia precisa de um caminho direto ou de um TURN.

## 1. Tunnel (o link HTTPS no seu domínio)

Pré-requisito: o domínio está na Cloudflare (DNS gerenciado por ela).

### Opção A: tunnel criado pelo painel (recomendada)

1. No painel da Cloudflare: **Zero Trust → Networks → Tunnels → Create a tunnel** (tipo *Cloudflared*). Dê um nome, ex.: `winshare-casa`.
2. A tela mostra um comando de instalação com um **token**. No PC que vai transmitir, instale o [cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) e rode, como administrador:
   ```bash
   cloudflared service install <TOKEN>
   ```
   Isso instala o tunnel como serviço do Windows, que sobe sozinho com o PC.
3. Em **Public Hostname**, adicione:
   - Subdomain: ex. `winshare`, Domain: o seu domínio;
   - Service: `HTTP` → `localhost:8080` (a `public_port`).
4. No `config.toml` desse PC:
   ```toml
   public_host = "winshare.seudominio.com.br"
   tunnel = "external"
   ```
   `external` quer dizer "o tunnel já roda por fora; o programa só monta o link".

### Opção B: o próprio programa sobe o tunnel (`managed`)

Para quem já usa o `cloudflared` pela linha de comando com tunnels locais:

```bash
cloudflared tunnel login
cloudflared tunnel create winshare
cloudflared tunnel route dns winshare winshare.seudominio.com.br
```

No `config.toml`:
```toml
public_host = "winshare.seudominio.com.br"
tunnel = "managed"
tunnel_name = "winshare"
```

O programa roda `cloudflared tunnel run --url http://127.0.0.1:<public_port> <tunnel_name>` e o derruba ao sair. Precisa do `cloudflared` no PATH e das credenciais criadas pelo `tunnel create` **naquele PC**.

### Opção C: sem tunnel

`tunnel = "none"`: o link usa `http://<public_host>:<public_port>`. Serve para rede local (ponha o IP do PC em `public_host`).

## 2. Caminho direto para o vídeo (portas UDP)

O WebRTC tenta primeiro ligar o espectador direto ao seu PC. Para isso, o seu PC precisa ser alcançável de fora:

1. **Saber se você está atrás de CGNAT.** Compare o IP WAN mostrado no roteador com o IP que um site como "qual é meu ip" mostra. Se forem diferentes (ou o do roteador começar com `100.64.`–`100.127.`), é CGNAT: as portas do roteador não chegam até você. Peça à operadora um **IPv4 público** (muitas fazem de graça ou com taxa pequena).
2. **Encaminhar as portas** UDP `50000–50100` (ou a faixa do `config.toml`) do roteador para o IP do PC.
3. **IPv6:** se a operadora entrega IPv6, espectadores com IPv6 podem conectar direto mesmo com CGNAT no IPv4. O programa já escuta em IPv6; libere a faixa UDP no firewall IPv6 do roteador se ele bloquear entrada.

IP dinâmico não é problema: a cada transmissão o STUN descobre o IP atual.

## 3. TURN (quando o direto não funciona)

O TURN repassa o vídeo para quem não consegue conexão direta (CGNAT, rede corporativa que bloqueia UDP). Ele é um protocolo padrão (RFC 8656); a Cloudflare oferece um serviço pronto.

### Cloudflare Realtime TURN

1. Painel da Cloudflare: **Realtime → TURN Server → Create**. (Não confunda com *Serverless SFU*, que gera um "App ID" diferente e não funciona aqui.)
2. Copie o **Turn Token ID** e o **API Token** (o token só aparece nessa hora).
3. No `config.toml`:
   ```toml
   [turn_cloudflare]
   key_id = "<Turn Token ID>"
   api_token = "<API Token>"
   ```
4. Reinicie o programa. No log aparece `credenciais TURN do Cloudflare obtidas`. Se aparecer `respondeu 404`, o ID não é de uma TURN key.

O programa pede credenciais válidas por 24 h ao iniciar a transmissão e renova a cada 6 h. O `api_token` nunca vai para o espectador nem para os logs.

**Custo** (preços da Cloudflare em 2026; confira a [página de preços](https://developers.cloudflare.com/realtime/sfu/pricing/)): os primeiros 1.000 GB por mês são grátis e depois US$ 0,05 por GB, contando só o tráfego que sai da Cloudflare para o espectador. Com o teto padrão de 3 Mbps, um espectador em relay gasta cerca de 1,35 GB por hora, ou seja, ~740 horas de espectador em relay por mês dentro da cota. Quem conecta direto não gasta nada.

### Outro servidor TURN

Qualquer servidor TURN padrão funciona (ex.: [coturn](https://github.com/coturn/coturn) num VPS):

```toml
[turn]
url = "turn:turn.seudominio.com.br:3478"
username = "usuario"
credential = "senha"
```

## Como saber qual caminho cada espectador usa

No painel (`/control`), a tabela **Espectadores** mostra a coluna **Caminho**: `Direto` ou `Relay` (TURN). O ICE sempre prefere o direto; o relay só é usado quando nenhum caminho direto responde.
