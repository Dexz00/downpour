"""Gera o `torrra.ico` a partir de `logo.png`.

    python assets/gerar-icone.py

A logo e larga e tem texto embaixo; um icone e quadrado e vai ser visto a 16
pixels. Entao o icone nao e um recorte simples: a pasta e isolada, o texto que
encosta no rodape e apagado, o brilho verde e reconstruido em volta e os
tamanhos pequenos usam um enquadramento mais fechado — o desenho inteiro
reduzido a 16 pixels vira borrao.

Depende de Pillow (`pip install pillow`). O `.ico` fica versionado, entao rodar
isto so e preciso quando a logo mudar.
"""

from PIL import Image, ImageDraw, ImageFilter, ImageChops
from pathlib import Path
import io
import struct

AQUI = Path(__file__).resolve().parent
ORIGEM = AQUI / "logo.png"
DESTINO = AQUI / "torrra.ico"

# Coordenadas na logo original (1536x1024).
LARGO = (488, 148, 1018, 548)   # a pasta inteira: aba, seta e simbolo
FECHADO = (545, 262, 900, 550)  # so a seta e o simbolo, para 16-24 px
SETA = (533, 718)               # faixa em x ocupada pela seta
FADE_DE, FADE_ATE = 512, 548    # onde o rodape (topo do texto) some
LADO = 1024
TAMANHOS = (16, 20, 24, 32, 40, 48, 64, 128, 256)


def recortar(origem, caixa):
    """Recorta e apaga o texto que invade o rodape, poupando a coluna da seta."""
    corte = origem.crop(caixa)
    w, h = corte.size

    mascara = Image.new("L", (w, h), 255)
    d = ImageDraw.Draw(mascara)
    y0, y1 = FADE_DE - caixa[1], FADE_ATE - caixa[1]
    for i in range(max(0, y0), h):
        v = 0 if i >= y1 else int(255 * (1 - (i - y0) / (y1 - y0)))
        d.line([(0, i), (w, i)], fill=v)
    d.rectangle([SETA[0] - caixa[0], 0, SETA[1] - caixa[0], h], fill=255)
    return Image.composite(corte, Image.new("RGB", (w, h), (0, 0, 0)), mascara)


def compor(corte, ocupacao=0.99):
    """Centraliza num quadrado preto e devolve o brilho verde ao fundo."""
    w, h = corte.size
    escala = (LADO * ocupacao) / max(w, h)
    novo = (int(w * escala), int(h * escala))
    corte = corte.resize(novo, Image.LANCZOS)
    px, py = (LADO - novo[0]) // 2, (LADO - novo[1]) // 2

    brilho = Image.new("RGB", (LADO, LADO), (0, 0, 0))
    brilho.paste(corte, (px, py))
    brilho = brilho.filter(ImageFilter.GaussianBlur(LADO // 12))
    r, g, b = brilho.split()
    tela = ImageChops.add(
        Image.new("RGB", (LADO, LADO), (0, 0, 0)),
        Image.merge("RGB", (r.point(lambda v: v // 3), g, b.point(lambda v: v // 3))),
    )
    tela.paste(corte, (px, py))
    return tela


def dib(imagem):
    """Bitmap de 32 bits com mascara AND zerada: o formato que o shell antigo le."""
    t = imagem.size[0]
    px = imagem.load()
    linhas = []
    for y in range(t - 1, -1, -1):                  # o DIB e gravado de baixo para cima
        linha = bytearray()
        for x in range(t):
            r, g, b, a = px[x, y]
            linha += bytes((b, g, r, a))
        linhas.append(bytes(linha))
    cabecalho = struct.pack("<IiiHHIIiiII", 40, t, t * 2, 1, 32, 0, t * t * 4, 0, 0, 0, 0)
    mascara = ((t + 31) // 32) * 4                  # cada linha alinhada a 4 bytes
    return cabecalho + b"".join(linhas) + b"\x00" * (mascara * t)


def main():
    origem = Image.open(ORIGEM).convert("RGB")
    largo = compor(recortar(origem, LARGO))
    fechado = compor(recortar(origem, FECHADO))

    entradas = []
    for t in TAMANHOS:
        fonte = fechado if t <= 24 else largo
        p = fonte.resize((t, t), Image.LANCZOS).convert("RGBA")
        if t >= 64:                                  # PNG so nos grandes
            buf = io.BytesIO()
            p.save(buf, format="PNG", optimize=True)
            entradas.append((t, buf.getvalue()))
        else:
            entradas.append((t, dib(p)))

    with open(DESTINO, "wb") as f:
        f.write(struct.pack("<HHH", 0, 1, len(entradas)))
        pos = 6 + 16 * len(entradas)
        for t, dados in entradas:
            # largura e altura sao um byte: 0 significa 256.
            f.write(struct.pack("<BBBBHHII", t % 256, t % 256, 0, 0, 1, 32, len(dados), pos))
            pos += len(dados)
        for _, dados in entradas:
            f.write(dados)

    print(f"{DESTINO.name}: {len(entradas)} tamanhos, {DESTINO.stat().st_size} bytes")


if __name__ == "__main__":
    main()
