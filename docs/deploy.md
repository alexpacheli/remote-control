# Servidor na VM (F3, marco 6)

Uma VM roda tudo o que o Hext Remote precisa na internet:

```
Internet ──443/TCP──► Caddy ─┬─ signal.hext.com.br ─► hext-server (127.0.0.1:8080)
                             └─ relay.hext.com.br  ─► iroh-relay  (127.0.0.1:3340)
         ──7842/UDP─────────────────────────────────► iroh-relay  (descoberta de endereço, QUIC)

iroh-relay ──"este computador pode usar o relay?"──► hext-server (/v1/relay-access)
```

- **Caddy** atende a porta 443 para os dois nomes, com certificados da Let's Encrypt renovados sozinhos.
- **`hext-server`** cuida dos IDs, da presença e dos pedidos de conexão (sinalização). O banco SQLite fica
  em `/var/lib/hext/hext-server.db`.
- **`iroh-relay`** repassa as sessões que não conseguem conexão direta. Ele **só aceita computadores
  registrados no `hext-server`**: a cada conexão, pergunta ao `hext-server`, apresentando um segredo
  guardado em `/etc/hext/secrets.env`. A porta UDP 7842 serve para cada computador descobrir o próprio
  endereço público, o que ajuda na conexão direta. Ela usa o certificado que o Caddy obtém.
- Nenhum dos serviços vê o conteúdo das sessões: o tráfego é cifrado de ponta a ponta entre os dois
  computadores.

## Primeira instalação

1. **DNS** (Cloudflare): crie um registro `A` para `signal` apontando para o IP da VM, igual ao de
   `relay`, e deixe em **"DNS only"** (nuvem cinza). O Caddy precisa receber a conexão direto para obter
   o certificado.
2. **Portas na Oracle** (Security List): TCP 80 e 443 e UDP 7842, as mesmas do relay. Se você abriu a
   TCP 8080 para o teste, pode remover.
3. **Na VM**, com o Rust já instalado (`rustup`):

   ```sh
   git clone https://github.com/alexpacheli/remote-control.git   # se ainda não clonou
   cd remote-control
   git pull
   ./deploy/install.sh
   ```

   O script:
   - instala o Caddy;
   - compila o `hext-server` e instala o `iroh-relay` 1.4.1;
   - gera o segredo do relay;
   - copia as configurações e os serviços do systemd;
   - libera as portas no iptables (e remove a 8080 do teste);
   - sobe tudo e testa os dois endereços.

   No fim, ele deve mostrar:

   ```
   ok    https://signal.hext.com.br/health
   ok    https://relay.hext.com.br/generate_204
   ```

   Na primeira vez, o `iroh-relay` pode reiniciar algumas vezes, até o Caddy obter o certificado (menos
   de um minuto).

A partir daí, o app usa esse servidor por padrão: `HextRemote host` mostra o ID, e
`HextRemote view <ID>` conecta, sem `--server`.

## Atualizar

```sh
cd remote-control && git pull && ./deploy/install.sh
```

O banco e o segredo continuam os mesmos. Os computadores reconectam sozinhos depois do reinício.

## Acompanhar

```sh
journalctl -u hext-server -u iroh-relay -u caddy -f      # logs
sudo systemctl status hext-server iroh-relay caddy        # estado
# auditoria (sudo apt install sqlite3)
sudo sqlite3 /var/lib/hext/hext-server.db \
  "SELECT datetime(at,'unixepoch'), from_id, to_id, event FROM audit ORDER BY id DESC LIMIT 20"
```

## Segurança

- O `hext-server` e o `iroh-relay` escutam só em `127.0.0.1`. Da internet chegam só o Caddy (80/443) e a
  descoberta de endereço do relay (UDP 7842).
- O `/v1/relay-access` é bloqueado no Caddy e exige o segredo: só o relay da própria VM pergunta.
- Limites contra tentativas de adivinhar IDs: por computador, por IP (pelo `X-Forwarded-For` do Caddy) e
  de computadores novos por IP (veja `docs/hext-server.md`).
- **Backup:** copie `/var/lib/hext/hext-server.db` de vez em quando. Ele guarda a ligação entre IDs e
  chaves; sem ele, os computadores ganham IDs novos.
