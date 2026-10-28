# Spike F3: vídeo sobre `iroh`

Marco 0 do [plano da F3](F3.md). A pergunta: o vídeo do Hext Remote funciona bem sobre o
[`iroh`](https://github.com/n0-computer/iroh), com o hole punching e o relay dele?

O `hext-spike` (`apps/hext-spike`) usa o mesmo empacotamento, FEC e remontagem do app, com quadros
sintéticos no formato do vídeo real (por padrão, 8 Mbps a 60 fps e um keyframe 10× maior a cada 2 s).
Ainda não usa captura nem encoder. Os relays são os públicos da n0; o relay próprio entra quando a VPS
estiver no ar.

## Como rodar

```sh
hext-spike host                 # mostra o ID
hext-spike view <ID>            # no outro computador
hext-spike view <ID> --relay-only   # o pior caso: tudo pelo relay
```

O host mostra, por segundo:

- fps e Mbps enviados;
- quadros pulados (quando a rede não dá conta);
- **quadro→ack**: o tempo desde começar a enviar um quadro até chegar a confirmação, em p50 e p95. É
  aproximadamente o RTT mais o tempo de transferir o quadro;
- o caminho em uso (direto ou relay) e o RTT.

O visualizador mostra fps, Mbps, FEC e quadros perdidos, além de quanto tempo levou para conectar e
para virar conexão direta.

## Matriz de testes

Rode cada cenário por ~1 minuto e anote as últimas linhas dos dois lados.

| # | Cenário | Caminho | Conectou em | Direto em | quadro→ack p50 / p95 | Perdas |
|---|---|---|---|---|---|---|
| 1 | Mesma rede (LAN) | | | | | |
| 2 | Casa ↔ outra casa | | | | | |
| 3 | Casa ↔ celular roteando 4G | | | | | |
| 4 | Igual ao 2, com `--relay-only` | relay | | — | | |
| 5 | Rede corporativa/restrita, se houver | | | | | |

## Critérios de decisão

- **Adotar o `iroh`:** fica direto nos cenários 1–3, ou pelo menos em 1–2; o relay entrega 8 Mbps com
  quadro→ack aceitável (< ~100 ms); e as perdas ficam baixas, com o FEC absorvendo o resto.
- **Implementação própria:** o `iroh` falha em conectar, nunca vira direto onde deveria, ou o vídeo
  sofre no relay a ponto de comprometer o uso.

## Resultados

_(preencher)_
