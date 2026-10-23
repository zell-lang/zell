package main

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"strconv"
	"strings"
	"time"
)

// encoding/json: a generated ~100 MB document parsed into typed structs
// (json.Unmarshal), formatted back (json.Marshal), parsed again schema-less
// (into any) with path lookups, and 1M small (~200 byte) API messages
// parsed and formatted one at a time. Same data as main.zl.
// SCALE=small runs a 1 MB document and 10k messages; JSON_DOC_BYTES=n
// sets the document size.

type Item struct {
	Id     int      `json:"id"`
	Name   string   `json:"name"`
	Price  float64  `json:"price"`
	Qty    int      `json:"qty"`
	Active bool     `json:"active"`
	Tags   []string `json:"tags"`
}

type Doc struct {
	Items []Item `json:"items"`
}

type Msg struct {
	Id     int      `json:"id"`
	User   string   `json:"user"`
	Email  string   `json:"email"`
	Action string   `json:"action"`
	Ts     int      `json:"ts"`
	Ok     bool     `json:"ok"`
	Score  float64  `json:"score"`
	Tags   []string `json:"tags"`
	Note   string   `json:"note"`
}

type Rng struct{ s int }

func (r *Rng) Next() int {
	r.s = (r.s*1103515245 + 12345) % 2147483648
	return r.s >> 8
}

var ws = []string{
	"lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do",
	"eiusmod", "tempor", "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "veniam",
}

func itoa(n int) string { return strconv.Itoa(n) }

func money(cents int) string {
	c := cents % 100
	if c < 10 {
		return itoa(cents/100) + ".0" + itoa(c)
	}
	return itoa(cents/100) + "." + itoa(c)
}

func genDoc(target int) string {
	r := Rng{12345}
	var b strings.Builder
	b.WriteString("{\"items\":[")
	i := 0
	for b.Len() < target {
		if i > 0 {
			b.WriteString(",\n")
		}
		b.WriteString("{\"id\":")
		b.WriteString(itoa(i*7 + 1))
		b.WriteString(",\"name\":\"")
		b.WriteString(ws[r.Next()%20])
		b.WriteString("-")
		b.WriteString(ws[r.Next()%20])
		b.WriteString("-")
		b.WriteString(itoa(i))
		b.WriteString("\",\"price\":")
		b.WriteString(money(r.Next() % 100000))
		b.WriteString(",\"qty\":")
		b.WriteString(itoa(r.Next() % 1000))
		if r.Next()%2 == 0 {
			b.WriteString(",\"active\":true,\"tags\":[")
		} else {
			b.WriteString(",\"active\":false,\"tags\":[")
		}
		nt := r.Next() % 4
		for t := 0; t < nt; t++ {
			if t > 0 {
				b.WriteString(",")
			}
			b.WriteString("\"")
			b.WriteString(ws[r.Next()%20])
			b.WriteString("\"")
		}
		b.WriteString("]}")
		i++
	}
	b.WriteString("]}")
	return b.String()
}

func genMsg(r *Rng, i int) string {
	u := r.Next() % 100000
	var b strings.Builder
	b.WriteString("{\"id\":")
	b.WriteString(itoa(i))
	b.WriteString(",\"user\":\"user-")
	b.WriteString(itoa(u))
	b.WriteString("\",\"email\":\"user")
	b.WriteString(itoa(u))
	b.WriteString("@example.com\",\"action\":\"")
	b.WriteString(ws[r.Next()%20])
	b.WriteString("\",\"ts\":")
	b.WriteString(itoa(1700000000 + r.Next()%100000000))
	b.WriteString(",\"ok\":true,\"score\":0.")
	b.WriteString(itoa(10 + r.Next()%90))
	b.WriteString(",\"tags\":[\"")
	b.WriteString(ws[r.Next()%20])
	b.WriteString("\",\"")
	b.WriteString(ws[r.Next()%20])
	b.WriteString("\"],\"note\":\"")
	b.WriteString(ws[r.Next()%20])
	b.WriteString(" ")
	b.WriteString(ws[r.Next()%20])
	b.WriteString(" ")
	b.WriteString(ws[r.Next()%20])
	b.WriteString("\"}")
	return b.String()
}

func checksum(d Doc) string {
	sumId, sumQty, sumCents, active, tags, nameBytes := 0, 0, 0, 0, 0, 0
	for _, it := range d.Items {
		sumId += it.Id
		sumQty += it.Qty
		sumCents += int(math.Round(it.Price * 100.0))
		if it.Active {
			active++
		}
		tags += len(it.Tags)
		nameBytes += len(it.Name)
	}
	return fmt.Sprintf("items=%d id=%d qty=%d cents=%d active=%d tags=%d names=%d",
		len(d.Items), sumId, sumQty, sumCents, active, tags, nameBytes)
}

func ms(d time.Duration) int64 { return d.Milliseconds() }

func mbs(bytes int, d time.Duration) int64 {
	n := d.Nanoseconds()
	if n <= 0 {
		n = 1
	}
	return int64(bytes) * 1000 / n
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	docSize := 100000000
	nMsgs := 1000000
	if os.Getenv("SCALE") == "small" {
		docSize = 1000000
		nMsgs = 10000
	}
	if v := os.Getenv("JSON_DOC_BYTES"); v != "" {
		n, err := strconv.Atoi(v)
		must(err)
		docSize = n
	}
	// JSON_DOC_FILE=path reads the document from that file instead of
	// generating it (and writes it there first if it does not exist), so
	// repeated runs and both implementations use one identical document.
	var text string
	if docFile := os.Getenv("JSON_DOC_FILE"); docFile != "" {
		if b, err := os.ReadFile(docFile); err == nil {
			text = string(b)
		} else {
			text = genDoc(docSize)
			must(os.WriteFile(docFile, []byte(text), 0o644))
		}
	} else {
		text = genDoc(docSize)
	}

	// 1. typed parse
	t0 := time.Now()
	var doc Doc
	must(json.Unmarshal([]byte(text), &doc))
	dParse := time.Since(t0)
	sum := checksum(doc)
	fmt.Println("typed:", sum)

	// 2. format back, check the round trip
	t2 := time.Now()
	out, err := json.Marshal(doc)
	must(err)
	dFormat := time.Since(t2)
	// The size is not printed: see main.zl.
	var again Doc
	must(json.Unmarshal(out, &again))
	fmt.Println("roundtrip:", checksum(again) == sum)

	// 3. schema-less value + path lookups
	t4 := time.Now()
	var v any
	must(json.Unmarshal([]byte(text), &v))
	dValue := time.Since(t4)
	t5 := time.Now()
	items := v.(map[string]any)["items"].([]any)
	sumId, sumCents, firstTags := 0, 0, 0
	for _, x := range items {
		it := x.(map[string]any)
		sumId += int(it["id"].(float64))
		sumCents += int(math.Round(it["price"].(float64) * 100.0))
		tg := it["tags"].([]any)
		if len(tg) > 0 {
			firstTags += len(tg[0].(string))
		}
	}
	dLookup := time.Since(t5)
	fmt.Println("value:", len(items), sumId, sumCents, firstTags)
	t6 := time.Now()
	vout, err := json.Marshal(v)
	must(err)
	dVFormat := time.Since(t6)
	fmt.Println("value format bytes:", len(vout))

	// 4. small messages
	r := Rng{777}
	msgs := make([]string, 0, 1024)
	for i := 0; i < 1024; i++ {
		msgs = append(msgs, genMsg(&r, i))
	}
	t8 := time.Now()
	acc, inBytes := 0, 0
	for i := 0; i < nMsgs; i++ {
		s := msgs[i%1024]
		inBytes += len(s)
		var m Msg
		must(json.Unmarshal([]byte(s), &m))
		acc += m.Id + m.Ts%1000 + len(m.User) + len(m.Tags) + len(m.Note)
		if m.Ok {
			acc++
		}
		acc += int(math.Round(m.Score * 100.0))
		f, err := json.Marshal(m)
		must(err)
		acc += len(f)
	}
	dMsgs := time.Since(t8)
	fmt.Println("messages:", nMsgs, acc, "avg bytes:", inBytes/nMsgs)

	e := os.Stderr
	fmt.Fprintf(e, "doc %d bytes\n", len(text))
	fmt.Fprintf(e, "typed parse   %d ms (%d MB/s)\n", ms(dParse), mbs(len(text), dParse))
	fmt.Fprintf(e, "typed format  %d ms (%d MB/s)\n", ms(dFormat), mbs(len(out), dFormat))
	fmt.Fprintf(e, "value parse   %d ms (%d MB/s)\n", ms(dValue), mbs(len(text), dValue))
	fmt.Fprintf(e, "value lookups %d ms\n", ms(dLookup))
	fmt.Fprintf(e, "value format  %d ms\n", ms(dVFormat))
	fmt.Fprintf(e, "messages      %d ms (%d msg/s, %d MB/s in)\n", ms(dMsgs), int64(nMsgs)*1000/(ms(dMsgs)+1), mbs(inBytes, dMsgs))
}
