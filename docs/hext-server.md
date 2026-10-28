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
 │─── hello {chave, assinatura, nome,   │   assinatura Ed25519 de "hext-signal-auth-v1\0" + nonce
 │           host ou técnico} ─────────►│
 │◄── welcome {id} ─────────────────────│   ID atribuído no primeiro login, o mesmo para sempre

Técnico                  Servidor                   Host
 │── connect {1, id} ──────►│                          │
 │                          │── request {r, de, chave, nome} ─►│  (o host pergunta ao usuário)
 │                          │◄── accept {r, endereços, código} ─│  ou reject {r}
 │◄─ accepted {1, chave do host, endereços, código} ─│         │  ou rejected {1, motivo}
```

- **Login:** o app prova que tem a chave assinando o desafio. O servidor associa a chave pública a um ID
  aleatório no primeiro login (não sequencial) e devolve sempre o mesmo ID para a mesma chave. Não há senha
  nem token guardado.
- **Host e técnico:** no login, o app diz se recebe pedidos (host) ou só faz pedidos (técnico). Um novo
  login de host com a mesma chave encerra o anterior (o computador reconectou); logins de técnico convivem
  com o host do mesmo computador.
- **Pedidos:** expiram em 60 s sem resposta (`rejected: timeout` para o técnico, `cancelled` para o host).
  Se o técnico sai, o host recebe `cancelled`; se o host sai, o técnico recebe `rejected: offline`. Só o
  host pedido pode responder.
- **Proteção:**
  - pedidos de conexão: até 10 por minuto por computador e 30 por endereço IP (`too_many_requests`). O
    limite não zera ao reconectar;
  - um pedido esperando por técnico e host, e até 5 por host (`busy`);
  - logins: até 30 por minuto por endereço IP; computadores novos (IDs entregues): até 10 por hora por IP;
  - ID inexistente responde igual a ID offline, para não dar para sondar quais IDs existem;
  - nomes sem caracteres de controle nem invisíveis (como os que invertem a direção do texto), com até
    64 caracteres, porque o nome do técnico aparece para quem está no host.
- **Auditoria:** a tabela `audit` registra quem pediu, para qual ID, quando e o resultado (`requested`,
  `accepted`, `declined`, `timeout`, `cancelled`, `offline`, `busy`, `rate_limited`).
- **Conexão viva:** o servidor manda ping a cada 20 s e desconecta quem fica 60 s em silêncio.

## Rodar

```sh
cargo run -p hext-server -- --listen 127.0.0.1:8080 --db hext-server.db
curl http://127.0.0.1:8080/health   # ok
```

O servidor fala HTTP simples. O TLS fica num proxy na frente, para os apps usarem `wss://`. Atrás do
proxy, use `--behind-proxy`: os limites por IP passam a usar o endereço que o proxy coloca em
`X-Forwarded-For`. Nesse caso, a porta do servidor precisa ficar acessível **só** para o proxy
(`127.0.0.1`), ou qualquer um poderia mandar um `X-Forwarded-For` falso.

### Docker

```sh
docker build -f apps/hext-server/Dockerfile -t hext-server .
docker run -d --name hext-server -p 127.0.0.1:8080:8080 -v hext-data:/data hext-server
```

A imagem já roda com `--behind-proxy`, por isso a porta é publicada só em `127.0.0.1`.

O banco fica no volume `/data` (`hext-server.db`). O backup é copiar esse arquivo.

### Na VM, atrás do Caddy

A instalação na VM (Caddy com TLS automático, `hext-server` e `iroh-relay`, que só aceita computadores
registrados) está em [deploy.md](deploy.md): um script faz tudo.

### Acesso ao relay

`POST /v1/relay-access` é o "porteiro" do `iroh-relay` (`access.http` na configuração dele). O relay manda
a chave do computador em `X-Iroh-Endpoint-Id` e o segredo em `Authorization: Bearer …`, o mesmo de
`--relay-token` ou `HEXT_RELAY_TOKEN`. A resposta é `200 true` se o computador tem um ID, `403` se não tem
e `401` com o segredo errado. Sem `--relay-token`, a rota responde `404`.
