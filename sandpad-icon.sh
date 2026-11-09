#!/usr/bin/env bash
# Badge an app icon with a mark ("#" by default) or a flag and install it
# into the user's icon theme, so a sandboxed launcher can be told apart from
# the real one — and, with different badges, from each other.
#
#   sandpad-icon.sh [-o NAME] [-m MARK | -f FLAG] ICON
#
# ICON is an icon name (looked up in the hicolor theme and /usr/share/pixmaps)
# or a path to a PNG/SVG. MARK is the text drawn in the badge: a character or
# two, e.g. "#", "W", "$", "₿" (longer text is shrunk to fit). FLAG fills the
# badge with a flag instead: a built-in one ("ru" — the Russian tricolour,
# "su" — the Soviet hammer and sickle, the emblem from Wikimedia Commons
# public-domain flag) or a path to a PNG/SVG of your own,
# cropped to the badge square. The result is installed as
# ~/.local/share/icons/hicolor/<size>x<size>/apps/NAME.png for the usual
# sizes (default NAME: <icon>-sandboxed) and the name is printed, ready for
# an Icon= line. Needs ImageMagick 7 (magick).
set -eu

usage() { echo "usage: sandpad-icon.sh [-o NAME] [-m MARK | -f FLAG] ICON" >&2; exit 2; }

NAME=""
MARK=""
FLAG=""
while getopts o:m:f:h OPT; do
  case "$OPT" in
    o) NAME="$OPTARG" ;;
    m) MARK="$OPTARG" ;;
    f) FLAG="$OPTARG" ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))
[ $# -eq 1 ] || usage
ICON="$1"
if [ -n "$MARK" ] && [ -n "$FLAG" ]; then
  echo "sandpad-icon.sh: -m and -f are exclusive" >&2; exit 2
fi
[ -n "$MARK" ] || [ -n "$FLAG" ] || MARK="#"

# resolve an icon name to its largest available file
SRC=""
if [ -f "$ICON" ]; then
  SRC="$ICON"
else
  for DIR in "$HOME/.local/share/icons" /usr/share/icons /var/lib/snapd/desktop/icons; do
    for F in "$DIR"/hicolor/scalable/apps/"$ICON".svg; do [ -f "$F" ] && SRC="$F" && break 2; done
    for S in 1024 512 256 128 96 64 48; do
      for F in "$DIR"/hicolor/${S}x${S}/apps/"$ICON".png; do [ -f "$F" ] && SRC="$F" && break 3; done
    done
  done
  [ -n "$SRC" ] || for F in /usr/share/pixmaps/"$ICON".png /usr/share/pixmaps/"$ICON".svg; do
    [ -f "$F" ] && SRC="$F" && break
  done
fi
[ -n "$SRC" ] || { echo "sandpad-icon.sh: icon not found: $ICON" >&2; exit 1; }
if [ -z "$NAME" ]; then
  NAME="$(basename "$ICON")"; NAME="${NAME%.*}-sandboxed"
fi

MASTER=512   # badge is drawn once at this size and scaled down with the icon
DEST="$HOME/.local/share/icons/hicolor"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# The badge: a rounded square in the bottom-right corner with a white rim,
# holding the mark in white on dark, or the flag — it covers ~40% of the edge
# so it survives downscaling. The source is drawn onto a MASTER-sized
# transparent canvas (SVGs rasterised at high density) so the badge geometry
# is the same for every icon.
B=$((MASTER * 40 / 100)); R=$((B / 5)); P=$((MASTER / 32))
X0=$((MASTER - P - B)); Y0=$((MASTER - P - B))
RIM=$((MASTER / 64))
if [ -n "$FLAG" ]; then
  # the flags are drawn as badge art — square, not in their real proportions
  case "$FLAG" in
    ru) cat > "$TMP/flag.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect width="100" height="33.4" fill="#fff"/>
  <rect y="33.3" width="100" height="33.4" fill="#0039a6"/>
  <rect y="66.6" width="100" height="33.4" fill="#d52b1e"/>
</svg>
EOF
      FLAG="$TMP/flag.svg" ;;
    su) cat > "$TMP/flag.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect width="100" height="100" fill="#cc0000"/>
  <g fill="#ffd700" transform="translate(50 50) scale(0.40) translate(-200 -151)">
    <path d="m 200.0005,37.5 -8.41933,25.911886 H 164.336 L 186.37777,79.426122 177.95844,105.338 200.0005,89.323465 222.04257,105.338 213.62324,79.426122 235.665,63.411886 h -27.24516 z m 0,13.499987 5.38828,16.583473 h 17.43718 l -14.107,10.249496 5.38827,16.583472 L 200.0005,84.167224 185.89378,94.416428 191.28205,77.832956 177.17504,67.58346 h 17.43718 z"/>
    <g transform="matrix(0.98931879,0,0,0.98673811,3.8297658,3.7659398)">
      <path d="m 137.43744,171.69421 18.86296,18.9937 17.78834,-17.66589 c 27.05847,29.021 55.43807,56.99501 82.28704,86.12782 4.03444,4.06233 10.59815,4.085 14.66056,0.0506 4.06232,-4.03445 4.08499,-10.59815 0.0506,-14.66056 -28.81871,-27.1901 -57.72545,-54.60143 -86.55328,-81.89095 l 23.96499,-23.80003 -33.34026,-4.61605 z"/>
      <path d="m 198.2887,110.1955 c 15.51743,8.7394 27.29872,21.28122 34.2484,34.3924 7.04394,13.28902 10.13959,27.16218 10.20325,38.25433 0.13054,22.74374 -18.43771,41.18184 -41.18183,41.18184 -12.13597,0 -23.04607,-5.24868 -30.58302,-13.60085 l -4.16863,3.51033 c -0.70999,-0.27231 -1.46387,-0.41221 -2.22429,-0.41276 -1.82948,1.9e-4 -3.56621,0.80531 -4.74859,2.20136 -2.97368,0.38896 -5.46251,2.44529 -6.40534,5.29224 -3.13486,6.28843 -8.63524,11.21997 -15.29104,13.4776 -0.0637,0.0216 -0.11992,0.05 -0.1758,0.0783 -3.07749,1.12758 -6.16259,3.1643 -8.78919,5.80245 -5.19155,5.23656 -7.72858,11.93658 -6.30024,16.63822 -0.14098,0.40857 -0.21361,0.83759 -0.21498,1.26979 1.5e-4,2.17082 1.75991,3.93058 3.93073,3.93073 0.54341,-0.002 1.08053,-0.11639 1.57745,-0.33632 4.69369,1.05881 11.06885,-1.54582 16.05444,-6.55917 2.82624,-2.85072 4.94356,-6.22349 5.98303,-9.53062 2.31696,-6.62278 7.29699,-12.01856 13.62281,-15.05312 0.15105,-0.0725 0.27303,-0.14714 0.38218,-0.22358 2.12082,-1.01408 3.67251,-2.92895 4.225,-5.2139 9.70222,11.44481 24.25255,18.75299 40.51876,19.13577 29.83352,0.70205 52.13299,-21.25802 53.16414,-52.83642 0.51894,-15.89259 -5.62993,-36.3847 -19.6412,-53.19089 -10.70835,-12.84441 -26.40987,-23.50795 -44.18699,-28.20777 z"/>
    </g>
  </g>
</svg>
EOF
      FLAG="$TMP/flag.svg" ;;
    *) [ -f "$FLAG" ] || { echo "sandpad-icon.sh: flag not found: $FLAG" >&2; exit 1; } ;;
  esac
  # the flag, scaled to cover the badge and cropped to its centre, clipped to
  # the rounded square (DstIn keeps the flag where the mask is opaque)
  magick -background none -density 300 "$FLAG" -density 72 \
    -resize "${B}x${B}^" -gravity center -extent "${B}x${B}" \
    \( -size "${B}x${B}" xc:none -fill white \
       -draw "roundrectangle 0,0 $((B - 1)),$((B - 1)) $R,$R" \) \
    -compose DstIn -composite "$TMP/badge.png"
  magick -size "${MASTER}x${MASTER}" xc:none \
    "$TMP/badge.png" -gravity northwest -geometry "+$X0+$Y0" -compose over -composite \
    -fill none -stroke white -strokewidth "$RIM" \
    -draw "roundrectangle $X0,$Y0 $((X0 + B)),$((Y0 + B)) $R,$R" \
    "$TMP/badge.png"
else
  # label: reads "%x" as an escape and a leading "@" as a file name
  LABEL="${MARK//%/%%}"; LABEL="${LABEL/#@/\\@}"
  # a bold font that has every character of the mark: DejaVu Sans when it does
  # (e.g. "#", letters, "$"), else whatever fontconfig finds with the glyphs
  CHARSET="$(printf '%s' "$MARK" | iconv -f UTF-8 -t UTF-32BE | od -An -tx1 -v | tr -d ' \n' | sed 's/.\{8\}/&,/g; s/,$//')"
  FONT="$(fc-match -f '%{file}' "DejaVu Sans:weight=bold:charset=$CHARSET")"
  magick -size "${MASTER}x${MASTER}" xc:none \
    -fill '#202020' -stroke white -strokewidth "$RIM" \
    -draw "roundrectangle $X0,$Y0 $((X0 + B)),$((Y0 + B)) $R,$R" \
    \( -size "$((B * 3 / 4))x$((B * 3 / 4))" -background none -fill white \
       -font "$FONT" -gravity center label:"$LABEL" \) \
    -gravity northwest -geometry "+$((X0 + B / 8))+$((Y0 + B / 8))" -compose over -composite \
    "$TMP/badge.png"
fi
magick -background none -density 300 "$SRC" -density 72 \
  -resize "${MASTER}x${MASTER}" -gravity center -extent "${MASTER}x${MASTER}" \
  "$TMP/badge.png" -compose over -composite \
  "$TMP/master.png"

for S in 512 256 128 96 64 48 32 24 22 16; do
  mkdir -p "$DEST/${S}x${S}/apps"
  magick "$TMP/master.png" -resize "${S}x${S}" "$DEST/${S}x${S}/apps/$NAME.png"
done
[ -f "$DEST/index.theme" ] || cp /usr/share/icons/hicolor/index.theme "$DEST/" 2>/dev/null || true
# GTK reads a prebuilt index; KDE has only a lazy runtime cache (KIconLoader),
# so instead tell running KDE apps to drop it — matters when re-badging an
# existing name or when a launcher was shown before its icon existed
gtk-update-icon-cache -q -t -f "$DEST" 2>/dev/null || true
dbus-send --session --type=signal /KIconLoader org.kde.KIconLoader.iconChanged int32:0 2>/dev/null || true
echo "$NAME"
