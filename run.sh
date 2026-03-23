cargo build --release
trap 'killall openferris; exit' INT
openferris daemon & openferris telegram & openferris gmail
killall openferris