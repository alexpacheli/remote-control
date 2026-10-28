# hext-server

O servidor dá a cada computador o seu **ID de 9 dígitos**, sabe quem está online e repassa os pedidos de
conexão entre eles (sinalização). Ele **nunca vê o conteúdo das sessões**: os computadores se conectam
direto entre si (ou por um relay), cifrados de ponta a ponta com as próprias chaves.

## Protocolo

Cada app mantém um WebSocket aberto em `wss://<servidor>/v1/signal`. As mensagens são JSON, marcadas por
`"type"`, e os tipos ficam em `crates/signal` (`ServerMsg` e `ClientMsg`).

```
App                                   Servidor
 │◄── challenge {protocol, nonce} ──────│   32 bytes aleatórios
 │─── hello {chave, assinatura, nome} ─►│   assinatura Ed25519 de "hext-signal-auth-v1\0" + nonce
 │◄── welcome {id} ─────────────────────│   ID atribuído no primeiro login, o mesmo para sempre

Técnico                  Servidor                   Host
 │── connect {1, id} ──────►│                          │
 │                          │── request {r, de, chave, nome} ─►│  (o host pergunta ao usuário)
 │                          │◄── accept {r, endereços} ────────│  ou reject {r}
 │◄─ accepted {1, chave do host, endereços} ─│                 │  ou rejected {1, motivo}
```

- **Login:** o app prova que tem a chave assinando o desafio. O servidor associa a chave pública a um ID
  aleatório no primeiro login (não sequencial) e devolve sempre o mesmo ID para a mesma chave. Não há senha
  nem token guardado.
- **Mesma chave conectando de novo:** a conexão antiga é encerrada.
- **Pedidos:** expiram em 60 s sem resposta (`rejected: timeout` para o técnico, `cancelled` para o host).
  Se o técnico sai, o host recebe `cancelled`; se o host sai, o técnico recebe `rejected: offline`. Só o
  host pedido pode responder.
- **Proteção:**
  - até 10 pedidos por minuto por conexão (`too_many_requests`);
  - até 5 pedidos esperando por host (`busy`);
  - ID inexistente responde igual a ID offline, para não dar para sondar quais IDs existem.
- **Auditoria:** a tabela `audit` registra quem pediu, para qual ID, quando e o resultado (`requested`,
  `accepted`, `declined`, `timeout`, `cancelled`, `offline`, `busy`, `rate_limited`).
- **Conexão viva:** o servidor manda ping a cada 20 s e desconecta quem fica 60 s em silêncio.

## Rodar

```sh
cargo run -p hext-server -- --listen 127.0.0.1:8080 --db hext-server.db
curl http://127.0.0.1:8080/health   # ok
```

O servidor fala HTTP simples. O TLS fica num proxy na frente, para os apps usarem `wss://`.

### Docker

```sh
docker build -f apps/hext-server/Dockerfile -t hext-server .
docker run -d --name hext-server -p 127.0.0.1:8080:8080 -v hext-data:/data hext-server
```

O banco fica no volume `/data` (`hext-server.db`). O backup é copiar esse arquivo.

### Caddy na frente (TLS automático)

```
signal.hext.com.br {
    reverse_proxy 127.0.0.1:8080
}
```

O Caddy obtém o certificado da Let's Encrypt sozinho e repassa o WebSocket. A subida na VPS, com domínio,
limites por IP e o app usando esse servidor por padrão, é o marco 6 da F3 ([F3.md](F3.md)).
