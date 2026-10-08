"""Regenerate form.pdf (test fixture) with ReportLab: python3 make_form.py"""
from reportlab.lib.pagesizes import letter
from reportlab.pdfgen import canvas

c = canvas.Canvas("form.pdf", pagesize=letter)
c.setTitle("Test form")
f = c.acroForm
c.drawString(72, 720, "Name:")
f.textfield(name="name", x=150, y=710, width=250, height=22, borderWidth=1, value="")
c.drawString(72, 680, "Notes:")
f.textfield(name="notes", x=150, y=620, width=250, height=70, fieldFlags="multiline", value="")
c.drawString(72, 590, "Subscribe:")
f.checkbox(name="subscribe", x=150, y=585, size=16, checked=False, buttonStyle="check")
c.drawString(72, 555, "Plan:")
f.radio(name="plan", value="basic", x=150, y=550, size=16, selected=True)
c.drawString(172, 555, "basic")
f.radio(name="plan", value="pro", x=230, y=550, size=16, selected=False)
c.drawString(252, 555, "pro")
c.drawString(72, 515, "Country:")
f.choice(name="country", x=150, y=505, width=150, height=22, options=["Japan", "India", "France"], value="Japan")
c.showPage()
c.drawString(72, 720, "Page 2 has no fields.")
c.save()
